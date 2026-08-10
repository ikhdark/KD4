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
    let row = sqlx::query("SELECT status, raw_text, summary, claim_token FROM bugs WHERE id = ?")
        .bind(created.id)
        .fetch_one(&store.pool)
        .await
        .expect("read row");
    assert_eq!(row.get::<String, _>("status"), "classified");
    assert_eq!(row.get::<String, _>("raw_text"), "immutable");
    assert_eq!(row.get::<String, _>("summary"), "summary");
    assert_eq!(row.get::<Option<String>, _>("claim_token"), None);
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
    let oldest = store.create(params("oldest", "one")).await.expect("oldest");
    let _next = store.create(params("next", "two")).await.expect("next");
    let new = store.create(params("new", "three")).await.expect("new");
    let claimed = store
        .claim_next_older(new.id)
        .await
        .expect("older claim")
        .expect("older row");
    assert_eq!(claimed.id, oldest.id);
    assert_ne!(claimed.id, new.id);

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
