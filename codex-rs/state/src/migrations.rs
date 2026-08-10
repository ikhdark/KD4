use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use sha2::Digest;
use sha2::Sha384;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

pub(crate) static STATE_MIGRATOR: Migrator = sqlx::migrate!("./migrations");
pub(crate) static LOGS_MIGRATOR: Migrator = sqlx::migrate!("./logs_migrations");
pub(crate) static GOALS_MIGRATOR: Migrator = sqlx::migrate!("./goals_migrations");
pub(crate) static MEMORIES_MIGRATOR: Migrator = sqlx::migrate!("./memory_migrations");
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

pub(crate) fn runtime_logs_migrator() -> Migrator {
    runtime_migrator(&LOGS_MIGRATOR)
}

pub(crate) fn runtime_goals_migrator() -> Migrator {
    runtime_migrator(&GOALS_MIGRATOR)
}

pub(crate) fn runtime_memories_migrator() -> Migrator {
    runtime_migrator(&MEMORIES_MIGRATOR)
}

pub(crate) fn runtime_bugs_migrator() -> Migrator {
    runtime_migrator(&BUGS_MIGRATOR)
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
    let migrations_table_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?
    .is_some();
    let applied = if migrations_table_exists {
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

pub(crate) async fn ensure_kd4_compatibility_indexes(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
CREATE INDEX IF NOT EXISTS idx_threads_cwd_norm
    ON threads(lower(replace(cwd, '\', '/')))
        "#,
    )
    .execute(pool)
    .await?;
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
    let migrations_table_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?
    .is_some();
    if !migrations_table_exists {
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
