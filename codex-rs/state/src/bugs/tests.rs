use super::*;

fn params<'a>(raw_text: &'a str, thread_id: &'a str) -> BugCreateParams<'a> {
    BugCreateParams {
        raw_text,
        thread_id,
        cwd: Some("C:/work\troot"),
        repository_root: Some("C:/work"),
        git_commit: Some("abc123"),
    }
}

fn classification<'a>() -> BugClassification<'a> {
    BugClassification {
        summary: "summary",
        severity: Some("high"),
        failure_mechanism: Some("crashes"),
        affected_components_json: "[\"src/main.rs\"]",
        stated_cause: Some("overflow"),
        required_repair: Some("check bounds"),
        classifier_provider_id: "provider",
        classifier_requested_model: "requested",
        classifier_resolved_model: Some("resolved"),
        classifier_reasoning_effort: "low",
        classifier_schema_version: "schema-v1",
        classifier_prompt_version: "prompt-v1",
    }
}

#[test]
fn bug_ids_are_zero_padded_without_truncating_large_values() {
    assert_eq!(format_bug_id(123), "B000123");
    assert_eq!(format_bug_id(1_234_567), "B1234567");
}

#[tokio::test]
async fn reopen_accepts_equivalent_checkout_line_endings_and_preserves_reports() {
    for crlf in [false, true] {
        let home = tempfile::tempdir().expect("temporary SQLite home");
        let store = BugStore::open(home.path()).await.expect("initial store");
        let created = store.create(params("preserved report 🐛", "thread-a"))
            .await.expect("persist report");
        let migration = &crate::migrations::BUGS_MIGRATOR.migrations[0];
        let sql = migration.sql.as_str().replace("\r\n", "\n");
        let sql = if crlf { sql.replace('\n', "\r\n") } else { sql };
        let checksum = <sha2::Sha384 as sha2::Digest>::digest(sql.as_bytes()).to_vec();
        sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 1")
            .bind(&checksum).execute(&store.pool).await.expect("checkout-equivalent ledger");
        store.pool.close().await;

        let reopened = BugStore::open(home.path()).await
            .expect("equivalent checkout must not make existing reports inaccessible");
        let claim = reopened.claim_by_id(created.id).await.expect("claim").expect("report");
        assert_eq!(claim.raw_text, "preserved report 🐛");
        assert_eq!(claim.attempt_count, 1);
        let persisted_checksum: Vec<u8> = sqlx::query_scalar(
            "SELECT checksum FROM _sqlx_migrations WHERE version = 1",
        ).fetch_one(&reopened.pool).await.expect("original ledger checksum");
        assert_eq!(persisted_checksum, checksum);
        reopened.pool.close().await;
    }
}

#[tokio::test]
async fn reopen_rejects_unrecognized_checksum_without_rewriting_reports_or_ledger() {
    let home = tempfile::tempdir().expect("temporary SQLite home");
    let store = BugStore::open(home.path()).await.expect("initial store");
    let created = store
        .create(params("preserved despite invalid ledger", "thread-a"))
        .await
        .expect("persist report");
    let checksum = <sha2::Sha384 as sha2::Digest>::digest(b"not the embedded migration").to_vec();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 1")
        .bind(&checksum)
        .execute(&store.pool)
        .await
        .expect("unrecognized ledger checksum");

    let error = match BugStore::open(home.path()).await {
        Ok(reopened) => {
            reopened.pool.close().await;
            panic!("modified migration must not be accepted");
        }
        Err(error) => error,
    };
    assert!(matches!(
        error.downcast_ref::<sqlx::migrate::MigrateError>(),
        Some(sqlx::migrate::MigrateError::VersionMismatch(1))
    ));
    let persisted_checksum: Vec<u8> =
        sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 1")
            .fetch_one(&store.pool)
            .await
            .expect("ledger retained");
    assert_eq!(persisted_checksum, checksum);
    let claim = store
        .claim_by_id(created.id)
        .await
        .expect("claim retained report")
        .expect("report retained");
    assert_eq!(claim.raw_text, "preserved despite invalid ledger");
    assert_eq!(claim.attempt_count, 1);
    store.pool.close().await;
}

#[tokio::test]
async fn concurrent_fresh_initializers_share_one_migration_and_reports() {
    let home = tempfile::tempdir().expect("temporary SQLite home");
    let (first, second) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(BugStore::open(home.path()), BugStore::open(home.path()))
    })
    .await
    .expect("concurrent initialization must finish");
    let first = first.expect("first initializer");
    let second = second.expect("second initializer");
    let migration_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&second.pool)
        .await
        .expect("migration ledger");
    assert_eq!(migration_count, 1);
    let created = first
        .create(params("shared report", "thread-a"))
        .await
        .expect("persist report");
    let claim = second
        .claim_by_id(created.id)
        .await
        .expect("claim from independent initializer")
        .expect("shared report");
    assert_eq!(claim.raw_text, "shared report");
    assert_eq!(claim.attempt_count, 1);
    first.pool.close().await;
    second.pool.close().await;
}

#[tokio::test]
async fn persistent_writer_lock_is_bounded_and_open_recovers_after_release() {
    use sqlx::Connection;

    let home = tempfile::tempdir().expect("temporary SQLite home");
    let options = SqliteConnectOptions::new()
        .filename(home.path().join(BUGS_DB_FILENAME))
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete);
    let mut writer = sqlx::SqliteConnection::connect_with(&options)
        .await
        .expect("blocking connection");
    sqlx::query("CREATE TABLE sentinel (value TEXT)")
        .execute(&mut writer)
        .await
        .expect("initialize rollback-journal database");
    sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut writer)
        .await
        .expect("hold exclusive writer lock");
    let blocked = tokio::time::timeout(Duration::from_secs(15), BugStore::open(home.path())).await;
    let rollback = sqlx::query("ROLLBACK").execute(&mut writer).await;
    let closed = writer.close().await;
    // Release the blocker even when the bounded-open assertion fails.
    rollback.expect("release writer lock");
    closed.expect("close blocking connection");
    let error = match blocked.expect("persistent contention must not wait indefinitely") {
        Ok(store) => {
            store.pool.close().await;
            panic!("exclusive writer lock must prevent opening the WAL database");
        }
        Err(error) => error,
    };
    assert!(matches!(
        error.downcast_ref::<sqlx::Error>(),
        Some(sqlx::Error::Database(error)) if error.code().as_deref() == Some("5")
    ));
    let store = BugStore::open(home.path()).await.expect("open after release");
    let created = store
        .create(params("report after lock release", "thread-a"))
        .await
        .expect("persist after release");
    let claim = store.claim_by_id(created.id).await.expect("claim").expect("report");
    assert_eq!(claim.raw_text, "report after lock release");
    store.pool.close().await;
}

#[tokio::test]
async fn exact_text_selected_home_and_independent_connection_claim_race() {
    let home = tempfile::tempdir().expect("temporary SQLite home");
    let first_store = BugStore::open(home.path()).await.expect("first store");
    let second_store = BugStore::open(home.path()).await.expect("second store");
    let raw_text = "  tabs\tCRLF\r\nUnicode e\u{301} 🐛  ";
    let created = first_store
        .create(params(raw_text, "thread-a"))
        .await
        .expect("insert");
    assert!(home.path().join(BUGS_DB_FILENAME).is_file());

    let (first, second) = tokio::join!(
        first_store.claim_by_id(created.id),
        second_store.claim_by_id(created.id),
    );
    let claims = [first.expect("first claim"), second.expect("second claim")];
    assert_eq!(claims.iter().filter(|claim| claim.is_some()).count(), 1);
    assert_eq!(
        claims
            .into_iter()
            .flatten()
            .next()
            .expect("winner")
            .raw_text,
        raw_text
    );
}

#[tokio::test]
async fn token_condition_stale_reclaim_and_three_attempt_exhaustion() {
    let home = tempfile::tempdir().expect("temporary SQLite home");
    let store = BugStore::open(home.path()).await.expect("store");
    let created = store
        .create(params("lease", "thread"))
        .await
        .expect("insert");
    let first = store
        .claim_by_id(created.id)
        .await
        .expect("claim")
        .expect("row");
    assert!(
        !store
            .release_failure(created.id, "wrong-token", BugFailureCategory::Provider)
            .await
            .expect("conditional release")
    );

    sqlx::query("UPDATE bugs SET claim_timestamp = ? WHERE id = ?")
        .bind(chrono::Utc::now().timestamp() - STALE_CLAIM_SECONDS - 1)
        .bind(created.id)
        .execute(&store.pool)
        .await
        .expect("age claim");
    let second = store
        .claim_by_id(created.id)
        .await
        .expect("reclaim")
        .expect("row");
    assert_eq!(second.attempt_count, 2);
    assert_ne!(first.claim_token, second.claim_token);
    assert!(
        !store
            .commit_classification(created.id, &first.claim_token, classification())
            .await
            .expect("reject former owner commit")
    );
    assert!(
        !store
            .release_failure(created.id, &first.claim_token, BugFailureCategory::Provider)
            .await
            .expect("reject former owner release")
    );
    assert!(
        store
            .release_failure(
                created.id,
                &second.claim_token,
                BugFailureCategory::Grounding
            )
            .await
            .expect("release")
    );
    let third = store
        .claim_by_id(created.id)
        .await
        .expect("third")
        .expect("row");
    assert_eq!(third.attempt_count, 3);
    assert!(
        store
            .release_failure(
                created.id,
                &third.claim_token,
                BugFailureCategory::Cancelled
            )
            .await
            .expect("release")
    );
    assert!(
        store
            .claim_by_id(created.id)
            .await
            .expect("fourth")
            .is_none()
    );
    let row = sqlx::query("SELECT status, attempt_count FROM bugs WHERE id = ?")
        .bind(created.id)
        .fetch_one(&store.pool)
        .await
        .expect("read row");
    assert_eq!(row.get::<String, _>("status"), "pending");
    assert_eq!(row.get::<i64, _>("attempt_count"), 3);
}

#[tokio::test]
async fn success_is_atomic_token_conditioned_and_submission_is_immutable() {
    let home = tempfile::tempdir().expect("temporary SQLite home");
    let store = BugStore::open(home.path()).await.expect("store");
    let created = store
        .create(params("immutable", "thread"))
        .await
        .expect("insert");
    let claim = store
        .claim_by_id(created.id)
        .await
        .expect("claim")
        .expect("row");
    assert!(
        store
            .release_failure(created.id, &claim.claim_token, BugFailureCategory::Provider)
            .await
            .expect("record failed attempt")
    );
    let claim = store
        .claim_by_id(created.id)
        .await
        .expect("reclaim")
        .expect("row");
    assert!(
        !store
            .commit_classification(created.id, "wrong-token", classification())
            .await
            .expect("conditional commit")
    );
    assert!(
        store
            .commit_classification(created.id, &claim.claim_token, classification())
            .await
            .expect("commit")
    );
    let row = sqlx::query("SELECT * FROM bugs WHERE id = ?")
        .bind(created.id)
        .fetch_one(&store.pool)
        .await
        .expect("read row");
    assert_eq!(row.get::<String, _>("status"), "classified");
    assert_eq!(row.get::<String, _>("raw_text"), "immutable");
    assert_eq!(row.get::<String, _>("summary"), "summary");
    assert_eq!(row.get::<Option<String>, _>("claim_token"), None);
    assert_eq!(row.get::<Option<i64>, _>("claim_timestamp"), None);
    assert_eq!(row.get::<Option<String>, _>("failure_category"), None);
    for (column, expected) in [
        ("severity", "high"),
        ("failure_mechanism", "crashes"),
        ("affected_components", "[\"src/main.rs\"]"),
        ("stated_cause", "overflow"),
        ("required_repair", "check bounds"),
        ("classifier_provider_id", "provider"),
        ("classifier_requested_model", "requested"),
        ("classifier_resolved_model", "resolved"),
        ("classifier_reasoning_effort", "low"),
        ("classifier_schema_version", "schema-v1"),
        ("classifier_prompt_version", "prompt-v1"),
    ] {
        assert_eq!(row.get::<String, _>(column), expected, "{column}");
    }
    assert!(
        sqlx::query("UPDATE bugs SET raw_text = 'changed' WHERE id = ?")
            .bind(created.id)
            .execute(&store.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn older_claim_excludes_new_id_and_orders_by_creation_then_id() {
    let home = tempfile::tempdir().expect("temporary SQLite home");
    let store = BugStore::open(home.path()).await.expect("store");
    let later = store.create(params("later", "one")).await.expect("later");
    let oldest = store.create(params("oldest", "two")).await.expect("oldest");
    let tied = store.create(params("tied", "three")).await.expect("tied");
    let new = store.create(params("excluded", "four")).await.expect("new");
    for (id, created_at) in [
        (later.id, 200),
        (oldest.id, 100),
        (tied.id, 100),
        (new.id, 0),
    ] {
        sqlx::query("UPDATE bugs SET created_at = ? WHERE id = ?")
            .bind(created_at)
            .bind(id)
            .execute(&store.pool)
            .await
            .expect("set creation order");
    }
    for expected_id in [oldest.id, tied.id, later.id] {
        let claimed = store
            .claim_next_older(new.id)
            .await
            .expect("older claim")
            .expect("row");
        assert_eq!(claimed.id, expected_id);
    }

    let only_new_home = tempfile::tempdir().expect("second SQLite home");
    let only_new_store = BugStore::open(only_new_home.path()).await.expect("store");
    let only_new = only_new_store
        .create(params("new", "only"))
        .await
        .expect("insert");
    assert!(
        only_new_store
            .claim_next_older(only_new.id)
            .await
            .expect("older claim")
            .is_none()
    );
}
