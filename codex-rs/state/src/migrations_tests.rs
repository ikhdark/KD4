use std::borrow::Cow;

use sqlx::Row;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

use super::MigrationLineEndings;
use super::STATE_MIGRATOR;
use super::ensure_kd4_compatibility_indexes;
use super::migration_checksum;
use super::migrator_with_line_endings;
use super::repair_legacy_recency_migration_version;
use super::runtime_migrator_for_pool;
use super::runtime_state_migrator;

fn migrator_through(version: i64) -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            STATE_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= version)
                .cloned()
                .collect(),
        ),
        ignore_missing: STATE_MIGRATOR.ignore_missing,
        locking: STATE_MIGRATOR.locking,
        table_name: STATE_MIGRATOR.table_name.clone(),
        create_schemas: STATE_MIGRATOR.create_schemas.clone(),
        no_tx: STATE_MIGRATOR.no_tx,
    }
}

async fn migration_ledger(
    pool: &sqlx::SqlitePool,
) -> Vec<(i64, String, String, i64, Vec<u8>, i64)> {
    sqlx::query_as(
        r#"
SELECT
    version,
    description,
    CAST(installed_on AS TEXT),
    success,
    checksum,
    execution_time
FROM _sqlx_migrations
ORDER BY version
        "#,
    )
    .fetch_all(pool)
    .await
    .expect("migration ledger should load")
}

#[tokio::test]
async fn recency_migration_backfills_and_seeds_old_binary_inserts() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory database should open");
    migrator_through(/*version*/ 37)
        .run(&pool)
        .await
        .expect("pre-recency migrations should apply");

    sqlx::query(
        r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000001")
    .bind("/tmp/first.jsonl")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_100_i64)
    .bind(1_700_000_000_123_i64)
    .bind(1_700_000_100_456_i64)
    .bind("cli")
    .bind("openai")
    .bind("/tmp")
    .bind("")
    .bind("read-only")
    .bind("on-request")
    .execute(&pool)
    .await
    .expect("legacy row should insert");

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("recency migration should apply");

    let backfilled = sqlx::query(
        "SELECT updated_at, updated_at_ms, recency_at, recency_at_ms FROM threads WHERE id = ?",
    )
    .bind("00000000-0000-0000-0000-000000000001")
    .fetch_one(&pool)
    .await
    .expect("backfilled row should load");
    assert_eq!(backfilled.get::<i64, _>("recency_at"), 1_700_000_100);
    assert_eq!(backfilled.get::<i64, _>("recency_at_ms"), 1_700_000_100_456);

    sqlx::query(
        r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000002")
    .bind("/tmp/second.jsonl")
    .bind(1_700_000_200_i64)
    .bind(1_700_000_300_i64)
    .bind(1_700_000_200_123_i64)
    .bind(1_700_000_300_456_i64)
    .bind("cli")
    .bind("openai")
    .bind("/tmp")
    .bind("")
    .bind("read-only")
    .bind("on-request")
    .execute(&pool)
    .await
    .expect("old-binary row should insert");

    let seeded = sqlx::query("SELECT recency_at, recency_at_ms FROM threads WHERE id = ?")
        .bind("00000000-0000-0000-0000-000000000002")
        .fetch_one(&pool)
        .await
        .expect("old-binary row should load");
    assert_eq!(seeded.get::<i64, _>("recency_at"), 1_700_000_300);
    assert_eq!(seeded.get::<i64, _>("recency_at_ms"), 1_700_000_300_456);
}

#[tokio::test]
async fn repairs_recency_migration_that_was_applied_as_version_38() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory database should open");
    migrator_through(/*version*/ 37)
        .run(&pool)
        .await
        .expect("pre-recency migrations should apply");

    let recency_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == 39)
        .expect("recency migration should exist");
    let mut legacy_migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 37)
        .cloned()
        .collect::<Vec<_>>();
    legacy_migrations.push(Migration::new(
        38,
        recency_migration.description.clone(),
        recency_migration.migration_type,
        recency_migration.sql.clone(),
        recency_migration.no_tx,
    ));
    let legacy_recency_migrator = Migrator::with_migrations(legacy_migrations);
    legacy_recency_migrator
        .run(&pool)
        .await
        .expect("legacy recency migration should apply as version 38");

    repair_legacy_recency_migration_version(&pool, &STATE_MIGRATOR)
        .await
        .expect("legacy migration history should be repaired");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply after repair");

    let applied = sqlx::query(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version >= 38 ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .expect("applied migrations should load")
    .into_iter()
    .map(|row| {
        (
            row.get::<i64, _>("version"),
            row.get::<Vec<u8>, _>("checksum"),
        )
    })
    .collect::<Vec<_>>();
    let expected = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version >= 38)
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(applied, expected);
}

#[tokio::test]
async fn runtime_migrator_preserves_the_complete_crlf_ledger() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory database should open");
    let crlf_migrator = migrator_with_line_endings(
        &migrator_through(/*version*/ 40),
        MigrationLineEndings::Crlf,
    );
    crlf_migrator
        .run(&pool)
        .await
        .expect("CRLF migration history should apply");
    for version in 41_i64..=44_i64 {
        sqlx::query(
            r#"
INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
VALUES (?, ?, TRUE, ?, 0)
            "#,
        )
        .bind(version)
        .bind(format!("upstream {version}"))
        .bind(vec![version as u8; 48])
        .execute(&pool)
        .await
        .expect("newer upstream migration row should insert");
    }
    let original_ledger = migration_ledger(&pool).await;

    let compatible = runtime_migrator_for_pool(&pool, &runtime_state_migrator())
        .await
        .expect("runtime migrator should inspect the existing history");
    compatible
        .run(&pool)
        .await
        .expect("line-ending-equivalent history should be accepted");
    ensure_kd4_compatibility_indexes(&pool)
        .await
        .expect("compatible index should be created after migration");
    ensure_kd4_compatibility_indexes(&pool)
        .await
        .expect("compatible index creation should be idempotent");

    assert_eq!(migration_ledger(&pool).await, original_ledger);
    assert!(
        STATE_MIGRATOR
            .migrations
            .iter()
            .all(|migration| migration.version <= 40),
        "fork-only indexes must not occupy the shared SQLx migration ledger"
    );
    let index_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'idx_threads_cwd_norm'",
    )
    .fetch_optional(&pool)
    .await
    .expect("index lookup should run")
    .is_some();
    assert!(index_exists);
}

#[tokio::test]
async fn runtime_migrator_uses_unambiguous_crlf_for_pending_migrations() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory database should open");
    migrator_with_line_endings(
        &migrator_through(/*version*/ 39),
        MigrationLineEndings::Crlf,
    )
    .run(&pool)
    .await
    .expect("CRLF migration history should apply");
    let original_ledger = migration_ledger(&pool).await;

    let compatible = runtime_migrator_for_pool(&pool, &runtime_state_migrator())
        .await
        .expect("one CRLF convention should be inferred");
    compatible
        .run(&pool)
        .await
        .expect("pending migration should apply");

    let migration = compatible
        .migrations
        .iter()
        .find(|migration| migration.version == 40)
        .expect("pending migration should exist");
    let applied_checksum = sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT checksum FROM _sqlx_migrations WHERE version = 40",
    )
    .fetch_one(&pool)
    .await
    .expect("pending migration checksum should load");
    assert_eq!(
        applied_checksum,
        migration_checksum(migration, MigrationLineEndings::Crlf)
    );
    let updated_ledger = migration_ledger(&pool).await;
    assert_eq!(
        &updated_ledger[..original_ledger.len()],
        original_ledger.as_slice()
    );
}

#[tokio::test]
async fn runtime_migrator_rejects_non_line_ending_checksum_changes() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory database should open");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply");
    sqlx::query("UPDATE _sqlx_migrations SET checksum = zeroblob(48) WHERE version = 1")
        .execute(&pool)
        .await
        .expect("test checksum should be corrupted");

    let compatible = runtime_migrator_for_pool(&pool, &runtime_state_migrator())
        .await
        .expect("runtime migrator should inspect the existing history");
    let err = compatible
        .run(&pool)
        .await
        .expect_err("real migration edits must remain rejected");
    assert!(matches!(
        err,
        sqlx::migrate::MigrateError::VersionMismatch(1)
    ));
}

#[tokio::test]
async fn runtime_migrator_rejects_mixed_line_ending_conventions() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory database should open");
    migrator_with_line_endings(&STATE_MIGRATOR, MigrationLineEndings::Crlf)
        .run(&pool)
        .await
        .expect("CRLF migration history should apply");
    let migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == 40)
        .expect("migration 40 should exist");
    sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 40")
        .bind(migration_checksum(migration, MigrationLineEndings::Lf))
        .execute(&pool)
        .await
        .expect("test history should mix checksum conventions");

    let err = runtime_migrator_for_pool(&pool, &runtime_state_migrator())
        .await
        .expect_err("mixed conventions must fail closed");
    assert!(
        err.to_string()
            .contains("mixes LF and CRLF checksum conventions")
    );
}
