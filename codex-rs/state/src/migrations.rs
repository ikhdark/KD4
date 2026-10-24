use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;

use sha2::Digest;
use sha2::Sha384;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

#[cfg(test)]
mod retirement_tests {
    use super::*;

    #[tokio::test]
    async fn goal_retirement_archives_before_drop_and_rolls_back_failed_preservation() {
        for fail_archive in [false, true] {
            let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
                .connect("sqlite::memory:").await.unwrap();
            sqlx::raw_sql("CREATE TABLE threads(id TEXT PRIMARY KEY); INSERT INTO threads VALUES ('thread');")
                .execute(&pool).await.unwrap();
            sqlx::raw_sql(include_str!("../migrations/0029_thread_goals.sql"))
                .execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO thread_goals VALUES ('thread', 'goal', 'keep objective', 'paused', 100, 42, 7, 1, 2)")
                .execute(&pool).await.unwrap();
            if fail_archive {
                sqlx::query("CREATE TABLE thread_goals_legacy_archive(wrong INTEGER)").execute(&pool).await.unwrap();
            }
            let migrator = Migrator {
                migrations: Cow::Owned(STATE_MIGRATOR.iter().filter(|migration| [33,34].contains(&migration.version)).cloned().collect()),
                ..runtime_state_migrator()
            };
            let migrated = run_state_migrations(&pool, &migrator).await;
            assert_eq!(migrated.is_err(), fail_archive);
            let query: &'static str = if fail_archive {
                "SELECT objective, tokens_used, time_used_seconds FROM thread_goals"
            } else {
                "SELECT objective, tokens_used, time_used_seconds FROM thread_goals_legacy_archive"
            };
            let row: (String, i64, i64) = sqlx::query_as(query)
                .fetch_one(&pool).await.unwrap();
            assert_eq!(row, ("keep objective".into(), 42, 7));
            if !fail_archive {
                run_state_migrations(&pool, &migrator).await.unwrap();
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM thread_goals_legacy_archive")
                    .fetch_one(&pool).await.unwrap();
                assert_eq!(count, 1);
            }
            pool.close().await;
        }
    }
}

pub(crate) static STATE_MIGRATOR: Migrator = sqlx::migrate!("./migrations");
pub(crate) static LOGS_MIGRATOR: Migrator = sqlx::migrate!("./logs_migrations");
pub(crate) static BUGS_MIGRATOR: Migrator = sqlx::migrate!("./bugs_migrations");

/// Allow an older Codex binary to open a database that has already been
/// migrated by a newer binary running in parallel.
///
/// We intentionally ignore applied migration versions that are newer than the
/// embedded migration set. Known migration versions are still validated by
/// checksum, so this only relaxes the "database is ahead of me" case.
fn runtime_migrator(base: &'static Migrator) -> Migrator {
    Migrator {
        migrations: Cow::Borrowed(base.migrations.as_ref()),
        ignore_missing: true,
        locking: base.locking,
        no_tx: base.no_tx,
        table_name: base.table_name.clone(),
        create_schemas: base.create_schemas.clone(),
    }
}

pub(crate) fn runtime_state_migrator() -> Migrator {
    runtime_migrator(&STATE_MIGRATOR)
}

/// Keep the deployed DROP and the compatibility restoration invisible until
/// both have committed. SQLx's inner migration transactions become savepoints.
pub(crate) async fn run_state_migrations(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let latest = migrator
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0);
    let current = migrations_table_exists(pool).await?
        && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version = ?)",
        )
        .bind(latest)
        .fetch_one(pool)
        .await?;
    if current {
        // Still validate known checksums; no writer reservation on warm opens.
        migrator.run(pool).await?;
    } else {
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        // Migration 34 intentionally retires the live API, not user data. Keep
        // the deployed SQL/checksum intact and archive before its DROP, within
        // the same migration transaction (never on the warm-open path).
        if migrator.iter().any(|migration| migration.version == 34) {
            // Older databases still need migration 33's goal-table rewrite.
            // Reach that boundary before preserving its final legacy schema.
            let before_retirement = Migrator {
                migrations: Cow::Owned(migrator.iter().filter(|migration| migration.version < 34).cloned().collect()),
                ignore_missing: true,
                locking: migrator.locking,
                no_tx: migrator.no_tx,
                table_name: migrator.table_name.clone(),
                create_schemas: migrator.create_schemas.clone(),
            };
            before_retirement.run_direct(None, &mut *transaction, false).await?;
        }
        let legacy_goals = migrator.iter().any(|migration| migration.version == 34)
            && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'thread_goals')",
        ).fetch_one(&mut *transaction).await?;
        if legacy_goals {
            sqlx::query("CREATE TABLE IF NOT EXISTS thread_goals_legacy_archive AS SELECT * FROM thread_goals WHERE 0")
                .execute(&mut *transaction).await?;
            sqlx::query("INSERT INTO thread_goals_legacy_archive SELECT * FROM thread_goals")
                .execute(&mut *transaction).await?;
            // The deployed DROP below retires the original only after the
            // copy; a failure rolls back both within this transaction.
        }
        migrator.run_direct(None, &mut *transaction, false).await?;
        transaction.commit().await?;
    }
    Ok(())
}

pub(crate) fn runtime_logs_migrator() -> Migrator {
    runtime_migrator(&LOGS_MIGRATOR)
}

pub(crate) fn runtime_bugs_migrator() -> Migrator {
    runtime_migrator(&BUGS_MIGRATOR)
}

/// SQLx takes no migration lock on SQLite, so concurrent initializers can both try to apply the
/// same pending migration and the later one fails replaying it. Retry while the ledger shows that
/// a writer made progress; a failure without ledger progress is returned unchanged.
pub async fn migrate_tolerating_concurrent_initializers<F, Fut>(
    pool: &SqlitePool,
    mut migrate: F,
) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    loop {
        let applied_before = applied_migration_count(pool).await.ok();
        let Err(err) = migrate().await else {
            return Ok(());
        };
        match (applied_before, applied_migration_count(pool).await.ok()) {
            (Some(before), Some(after)) if after > before => {}
            _ => return Err(err),
        }
    }
}

async fn applied_migration_count(pool: &SqlitePool) -> anyhow::Result<i64> {
    if !migrations_table_exists(pool).await? {
        return Ok(0);
    }
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(pool)
            .await?,
    )
}

async fn migrations_table_exists(pool: &SqlitePool) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?
    .is_some())
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum MigrationLineEndings {
    Lf,
    Crlf,
}

fn migration_checksum(
    migration: &sqlx::migrate::Migration,
    line_endings: MigrationLineEndings,
) -> Vec<u8> {
    let lf_sql = migration.sql.as_str().replace("\r\n", "\n");
    let normalized_sql = match line_endings {
        MigrationLineEndings::Lf => lf_sql,
        MigrationLineEndings::Crlf => lf_sql.replace('\n', "\r\n"),
    };
    Sha384::digest(normalized_sql.as_bytes()).to_vec()
}

fn matching_line_endings(
    migration: &sqlx::migrate::Migration,
    checksum: &[u8],
) -> Vec<MigrationLineEndings> {
    [MigrationLineEndings::Lf, MigrationLineEndings::Crlf]
        .into_iter()
        .filter(|line_endings| migration_checksum(migration, *line_endings) == checksum)
        .collect()
}

#[cfg(test)]
fn migrator_with_line_endings(migrator: &Migrator, line_endings: MigrationLineEndings) -> Migrator {
    let migrations = migrator
        .migrations
        .iter()
        .cloned()
        .map(|mut migration| {
            migration.checksum = Cow::Owned(migration_checksum(&migration, line_endings));
            migration
        })
        .collect();
    Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: migrator.ignore_missing,
        locking: migrator.locking,
        no_tx: migrator.no_tx,
        table_name: migrator.table_name.clone(),
        create_schemas: migrator.create_schemas.clone(),
    }
}

/// Make migration checksums portable across Git's LF/CRLF checkout modes.
///
/// SQLx hashes the migration file bytes, so semantically identical SQL checked
/// out with different line endings otherwise fails as a modified migration.
/// Applied rows are never rewritten. We accept only an exact LF or CRLF hash
/// of the embedded SQL. Pending migrations inherit a convention only when the
/// recognized applied history is consistent and unambiguous.
pub(crate) async fn runtime_migrator_for_pool(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<Migrator> {
    let applied = if migrations_table_exists(pool).await? {
        sqlx::query_as::<_, (i64, Vec<u8>)>(
            "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect::<BTreeMap<_, _>>()
    } else {
        BTreeMap::new()
    };

    let mut recognized_checksums = BTreeMap::new();
    let mut recognized_line_endings = BTreeSet::new();
    for migration in migrator.migrations.iter() {
        let Some(applied_checksum) = applied.get(&migration.version) else {
            continue;
        };
        let matching = matching_line_endings(migration, applied_checksum);
        if migration.checksum.as_ref() == applied_checksum.as_slice() || !matching.is_empty() {
            recognized_checksums.insert(migration.version, applied_checksum.clone());
            if let [line_endings] = matching.as_slice() {
                recognized_line_endings.insert(*line_endings);
            }
        }
    }

    if recognized_line_endings.len() > 1 {
        anyhow::bail!("applied migration history mixes LF and CRLF checksum conventions");
    }
    let has_pending_migrations = migrator
        .migrations
        .iter()
        .any(|migration| !applied.contains_key(&migration.version));
    if !applied.is_empty() && has_pending_migrations && recognized_line_endings.len() != 1 {
        anyhow::bail!("cannot infer one unambiguous checksum convention for pending migrations");
    }
    let preferred_line_endings = recognized_line_endings.first().copied();

    let mut compatible = Migrator {
        migrations: Cow::Owned(migrator.migrations.iter().cloned().collect()),
        ignore_missing: migrator.ignore_missing,
        locking: migrator.locking,
        no_tx: migrator.no_tx,
        table_name: migrator.table_name.clone(),
        create_schemas: migrator.create_schemas.clone(),
    };
    for migration in compatible.migrations.to_mut() {
        if let Some(applied_checksum) = recognized_checksums.get(&migration.version) {
            migration.checksum = Cow::Owned(applied_checksum.clone());
            continue;
        }
        if !applied.contains_key(&migration.version)
            && let Some(line_endings) = preferred_line_endings
        {
            migration.checksum = Cow::Owned(migration_checksum(migration, line_endings));
        }
    }
    Ok(compatible)
}

/// Repair the short-lived KD4 migration ordering where the thread/agent-job
/// indexes were recorded as version 41 and validation history as version 42.
///
/// The repair is deliberately checksum-gated. It recognizes only the exact LF
/// or CRLF forms of the SQL that is now embedded as versions 42 and 41,
/// respectively, and changes only the SQLx ledger. The schema objects already
/// created by those migrations are unchanged.
pub(crate) async fn repair_legacy_validation_index_migration_order(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    repair_legacy_validation_index_migration_order_inner(
        pool,
        migrator,
        #[cfg(test)]
        None,
    )
    .await
}

async fn repair_legacy_validation_index_migration_order_inner(
    pool: &SqlitePool,
    migrator: &Migrator,
    #[cfg(test)] after_preflight: Option<&tokio::sync::Barrier>,
) -> anyhow::Result<()> {
    let Some(validation_migration) = migrator
        .migrations
        .iter()
        .find(|migration| migration.version == 41)
    else {
        return Ok(());
    };
    let Some(index_migration) = migrator
        .migrations
        .iter()
        .find(|migration| migration.version == 42)
    else {
        return Ok(());
    };
    if !migrations_table_exists(pool).await? {
        return Ok(());
    }

    let rows = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version IN (41, 42) ORDER BY version",
    )
    .fetch_all(pool)
    .await?;
    if !rows.iter().any(|(version, checksum)| {
        *version == 41 && !matching_line_endings(index_migration, checksum).is_empty()
    }) {
        return Ok(());
    }

    #[cfg(test)]
    if let Some(barrier) = after_preflight {
        barrier.wait().await;
    }

    // Preflight is advisory: another initializer may repair the ledger while
    // we wait for the writer. Recognize and mutate the same protected state.
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let rows = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version IN (41, 42) ORDER BY version",
    )
    .fetch_all(&mut *transaction)
    .await?;
    let checksum_41 = rows
        .iter()
        .find_map(|(version, checksum)| (*version == 41).then_some(checksum.as_slice()));
    let checksum_42 = rows
        .iter()
        .find_map(|(version, checksum)| (*version == 42).then_some(checksum.as_slice()));
    let version_41_is_legacy_indexes = checksum_41
        .is_some_and(|checksum| !matching_line_endings(index_migration, checksum).is_empty());
    if !version_41_is_legacy_indexes {
        return Ok(());
    }

    let version_42_is_legacy_validation = checksum_42
        .is_some_and(|checksum| !matching_line_endings(validation_migration, checksum).is_empty());
    if checksum_42.is_some() && !version_42_is_legacy_validation {
        anyhow::bail!(
            "legacy migration 41 checksum matched thread/agent-job indexes, but migration 42 was not the recognized validation-history migration"
        );
    }

    if version_42_is_legacy_validation {
        sqlx::query("UPDATE _sqlx_migrations SET version = -41 WHERE version = 41")
            .execute(&mut *transaction)
            .await?;
        sqlx::query("UPDATE _sqlx_migrations SET version = 41, description = ? WHERE version = 42")
            .bind(validation_migration.description.as_ref())
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "UPDATE _sqlx_migrations SET version = 42, description = ? WHERE version = -41",
        )
        .bind(index_migration.description.as_ref())
        .execute(&mut *transaction)
        .await?;
    } else {
        sqlx::query("UPDATE _sqlx_migrations SET version = 42, description = ? WHERE version = 41")
            .bind(index_migration.description.as_ref())
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(())
}

pub(crate) async fn repair_legacy_recency_migration_version(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let Some(recency_migration) = migrator
        .migrations
        .iter()
        .find(|migration| migration.version == 39)
    else {
        return Ok(());
    };
    if !migrations_table_exists(pool).await? {
        return Ok(());
    }

    // Even a no-op UPDATE requires SQLite's writer slot. Keep current histories
    // read-only, and retain the atomic predicates below if another initializer repairs first.
    let needs_repair = sqlx::query_scalar::<_, i64>(
        r#"
SELECT 1 FROM _sqlx_migrations
WHERE version = ? AND checksum = ?
  AND NOT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = ?)
        "#,
    )
    .bind(38_i64)
    .bind(recency_migration.checksum.as_ref())
    .bind(recency_migration.version)
    .fetch_optional(pool)
    .await?
    .is_some();
    if !needs_repair {
        return Ok(());
    }

    sqlx::query(
        r#"
UPDATE _sqlx_migrations
SET version = ?, description = ?
WHERE version = ?
  AND checksum = ?
  AND NOT EXISTS (
      SELECT 1 FROM _sqlx_migrations WHERE version = ?
  )
        "#,
    )
    .bind(recency_migration.version)
    .bind(recency_migration.description.as_ref())
    .bind(38_i64)
    .bind(recency_migration.checksum.as_ref())
    .bind(recency_migration.version)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
