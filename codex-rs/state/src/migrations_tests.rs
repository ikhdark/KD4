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
use super::repair_legacy_validation_index_migration_order;
use super::runtime_migrator_for_pool;
use super::runtime_state_migrator;

const HISTORICAL_VALIDATION_HISTORY_41_LF_CHECKSUM: [u8; 48] = [
    0x44, 0x70, 0xf6, 0x8c, 0xf6, 0xf7, 0x0d, 0xc5, 0xb1, 0xbe, 0xa9, 0x19, 0xc8, 0xbf, 0x86, 0xe8,
    0x56, 0xd6, 0x36, 0xc2, 0x15, 0x8f, 0xb8, 0x94, 0xfa, 0x64, 0x6f, 0x40, 0x07, 0x56, 0xdf, 0xd7,
    0x1b, 0x49, 0x83, 0xe1, 0xf2, 0xf0, 0xca, 0xcf, 0x4c, 0x24, 0x04, 0xb8, 0xc4, 0xfa, 0x5a, 0xef,
];
const HISTORICAL_VALIDATION_HISTORY_41_CRLF_CHECKSUM: [u8; 48] = [
    0x5c, 0xdf, 0x7e, 0xfc, 0xbf, 0x99, 0xb6, 0xd8, 0x78, 0x16, 0xd0, 0xaa, 0x05, 0x7d, 0x79, 0x96,
    0xc6, 0x5a, 0x52, 0xa0, 0xfe, 0xb0, 0x1d, 0x8e, 0x7c, 0x45, 0x52, 0x20, 0x1c, 0x8b, 0x5f, 0x92,
    0x93, 0xa2, 0xb4, 0xa9, 0xf4, 0xe2, 0xf6, 0x18, 0x48, 0xc8, 0xda, 0x41, 0x0e, 0x25, 0x64, 0xbe,
];

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
    let crlf_migrator = migrator_with_line_endings(&STATE_MIGRATOR, MigrationLineEndings::Crlf);
    crlf_migrator
        .run(&pool)
        .await
        .expect("CRLF migration history should apply");
    let latest_known_version = STATE_MIGRATOR
        .migrations
        .iter()
        .map(|migration| migration.version)
        .max()
        .expect("state migrations should not be empty");
    for version in (latest_known_version + 1)..=(latest_known_version + 4) {
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
    assert_eq!(latest_known_version, 43);
    assert_eq!(
        STATE_MIGRATOR
            .migrations
            .iter()
            .find(|migration| migration.version == 41)
            .map(|migration| migration.description.as_ref()),
        Some("validation history")
    );
    assert_eq!(
        STATE_MIGRATOR
            .migrations
            .iter()
            .find(|migration| migration.version == 42)
            .map(|migration| migration.description.as_ref()),
        Some("thread and agent job indexes")
    );
    assert_eq!(
        STATE_MIGRATOR
            .migrations
            .iter()
            .find(|migration| migration.version == 43)
            .map(|migration| migration.description.as_ref()),
        Some("threads visible sort tiebreaker indexes")
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
async fn historical_validation_history_41_upgrades_without_checksum_mismatch_or_data_loss() {
    let historical_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == 41)
        .expect("historical migration 41 should exist");
    assert_eq!(
        historical_migration.description.as_ref(),
        "validation history"
    );
    assert_eq!(
        migration_checksum(historical_migration, MigrationLineEndings::Lf),
        HISTORICAL_VALIDATION_HISTORY_41_LF_CHECKSUM
    );
    assert_eq!(
        migration_checksum(historical_migration, MigrationLineEndings::Crlf),
        HISTORICAL_VALIDATION_HISTORY_41_CRLF_CHECKSUM
    );

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("historical in-memory database should open");
    migrator_through(/*version*/ 41)
        .run(&pool)
        .await
        .expect("historical migrations through validation history should apply");
    sqlx::query(
        r#"
INSERT INTO validation_history_aggregates (
    scope_kind,
    repository_id,
    fingerprint_id,
    operation,
    ecosystem,
    breadth,
    model_version,
    key_version,
    completed_count,
    censored_below_count,
    censored_above_count,
    duration_sum_ms,
    duration_sum_squares_ms,
    updated_at
) VALUES (1, 'historical-repository', 'historical-fingerprint', 2, 3, 4, 5, 6, 7, 8, 9, 10.5, 11.5, 12)
        "#,
    )
    .execute(&pool)
    .await
    .expect("historical validation aggregate should insert");
    let historical_ledger_row = migration_ledger(&pool)
        .await
        .into_iter()
        .find(|row| row.0 == 41)
        .expect("historical migration 41 ledger row should exist");

    runtime_migrator_for_pool(&pool, &runtime_state_migrator())
        .await
        .expect("current migrator should accept historical migration 41")
        .run(&pool)
        .await
        .expect("historical database should upgrade through current migrations");

    let upgraded_ledger_row = migration_ledger(&pool)
        .await
        .into_iter()
        .find(|row| row.0 == 41)
        .expect("upgraded migration 41 ledger row should remain");
    assert_eq!(upgraded_ledger_row, historical_ledger_row);
    let preserved = sqlx::query_as::<_, (i64, i64, i64, f64, f64, i64)>(
        r#"
SELECT
    completed_count,
    censored_below_count,
    censored_above_count,
    duration_sum_ms,
    duration_sum_squares_ms,
    updated_at
FROM validation_history_aggregates
WHERE repository_id = 'historical-repository'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("historical validation aggregate should remain");
    assert_eq!(preserved, (7, 8, 9, 10.5, 11.5, 12));

    let fresh_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("fresh in-memory database should open");
    STATE_MIGRATOR
        .run(&fresh_pool)
        .await
        .expect("fresh database should reach the current schema");
    let schema_query = r#"
SELECT type, name, tbl_name
FROM sqlite_master
WHERE name IN (
    'validation_history_aggregates',
    'validation_history_aggregates_updated_at_idx',
    'idx_agent_job_items_job_row',
    'idx_threads_cwd_norm'
)
ORDER BY type, name
    "#;
    let upgraded_schema = sqlx::query_as::<_, (String, String, String)>(schema_query)
        .fetch_all(&pool)
        .await
        .expect("upgraded schema should load");
    let fresh_schema = sqlx::query_as::<_, (String, String, String)>(schema_query)
        .fetch_all(&fresh_pool)
        .await
        .expect("fresh schema should load");
    assert_eq!(upgraded_schema, fresh_schema);
    assert_eq!(upgraded_schema.len(), 4);
}

fn legacy_validation_index_migrator(include_validation: bool) -> Migrator {
    let validation = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == 41)
        .expect("validation migration should exist");
    let indexes = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == 42)
        .expect("index migration should exist");
    let mut migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 40)
        .cloned()
        .collect::<Vec<_>>();
    migrations.push(Migration::new(
        41,
        indexes.description.clone(),
        indexes.migration_type,
        indexes.sql.clone(),
        indexes.no_tx,
    ));
    if include_validation {
        migrations.push(Migration::new(
            42,
            validation.description.clone(),
            validation.migration_type,
            validation.sql.clone(),
            validation.no_tx,
        ));
    }
    Migrator::with_migrations(migrations)
}

#[tokio::test]
async fn repairs_swapped_validation_and_index_migration_versions() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("legacy in-memory database should open");
    legacy_validation_index_migrator(true)
        .run(&pool)
        .await
        .expect("legacy migration ordering should apply");
    sqlx::query(
        r#"
INSERT INTO validation_history_aggregates (
    scope_kind, repository_id, fingerprint_id, operation, ecosystem, breadth,
    model_version, key_version, completed_count, updated_at
) VALUES (1, 'legacy-repository', 'legacy-fingerprint', 2, 3, 4, 5, 6, 7, 8)
        "#,
    )
    .execute(&pool)
    .await
    .expect("legacy validation data should insert");

    repair_legacy_validation_index_migration_order(&pool, &STATE_MIGRATOR)
        .await
        .expect("recognized legacy ledger should repair");
    repair_legacy_validation_index_migration_order(&pool, &STATE_MIGRATOR)
        .await
        .expect("ledger repair should be idempotent");
    runtime_migrator_for_pool(&pool, &runtime_state_migrator())
        .await
        .expect("repaired ledger should be compatible")
        .run(&pool)
        .await
        .expect("current migrations should accept repaired ledger");

    let versions_and_checksums = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version IN (41, 42) ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .expect("repaired ledger should load");
    let expected = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| matches!(migration.version, 41 | 42))
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(versions_and_checksums, expected);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT completed_count FROM validation_history_aggregates WHERE repository_id = 'legacy-repository'",
        )
        .fetch_one(&pool)
        .await
        .expect("validation data should remain"),
        7
    );
}

#[tokio::test]
async fn repairs_legacy_index_41_before_validation_was_applied() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("legacy in-memory database should open");
    legacy_validation_index_migrator(false)
        .run(&pool)
        .await
        .expect("legacy index migration should apply");

    repair_legacy_validation_index_migration_order(&pool, &STATE_MIGRATOR)
        .await
        .expect("recognized partial legacy ledger should repair");
    runtime_migrator_for_pool(&pool, &runtime_state_migrator())
        .await
        .expect("repaired ledger should be compatible")
        .run(&pool)
        .await
        .expect("current migrations should apply validation history");

    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE version IN (41, 42)",
        )
        .fetch_one(&pool)
        .await
        .expect("ledger count should load"),
        2
    );
    assert!(
        sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'validation_history_aggregates'",
        )
        .fetch_optional(&pool)
        .await
        .expect("schema should load")
        .is_some()
    );
}

#[tokio::test]
async fn rejects_unknown_legacy_validation_checksum_without_rewriting_the_ledger() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("legacy in-memory database should open");
    legacy_validation_index_migrator(true)
        .run(&pool)
        .await
        .expect("legacy migration ordering should apply");
    sqlx::query("UPDATE _sqlx_migrations SET checksum = zeroblob(48) WHERE version = 42")
        .execute(&pool)
        .await
        .expect("test checksum should be corrupted");
    let original_ledger = migration_ledger(&pool).await;

    let err = repair_legacy_validation_index_migration_order(&pool, &STATE_MIGRATOR)
        .await
        .expect_err("an unknown validation checksum must fail closed");

    assert!(
        err.to_string()
            .contains("migration 42 was not the recognized validation-history migration")
    );
    assert_eq!(migration_ledger(&pool).await, original_ledger);
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
