use chrono::Duration;
use chrono::Utc;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

static TEST_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");


fn task_store_migrator_through(version: i64) -> sqlx::migrate::Migrator {
    sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            TEST_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= version)
                .cloned()
                .collect(),
        ),
        ignore_missing: TEST_MIGRATOR.ignore_missing,
        locking: TEST_MIGRATOR.locking,
        table_name: TEST_MIGRATOR.table_name.clone(),
        create_schemas: TEST_MIGRATOR.create_schemas.clone(),
        no_tx: TEST_MIGRATOR.no_tx,
    }
}

use super::*;
use crate::local::TestSnapshotCapturePause;
use crate::local::with_test_snapshot_capture_pause;

#[test]
fn editing_and_tool_calls_are_meaningful_progress() {
    assert!(ObservationKind::Editing.is_meaningful_progress());
    assert!(ObservationKind::ToolCall.is_meaningful_progress());
    assert!(!ObservationKind::Starting.is_meaningful_progress());
}

#[test]
fn embedded_migration_checksums_match_persisted_line_endings() {
    use sha2::Digest;
    // SQLx hashes raw file bytes. Version 20 was first applied from CRLF bytes, and
    // `.gitattributes` pins that checkout form; every other migration is embedded as LF.
    for migration in TEST_MIGRATOR.migrations.iter() {
        let lf_sql = migration.sql.as_str().replace("\r\n", "\n");
        let persisted_sql = if migration.version == 20 {
            lf_sql.replace('\n', "\r\n")
        } else {
            lf_sql
        };
        assert_eq!(
            migration.checksum.as_ref(),
            sha2::Sha384::digest(persisted_sql.as_bytes()).as_slice(),
            "migration {} must embed the line endings recorded by persisted ledgers",
            migration.version
        );
    }
}




#[tokio::test]
async fn agent_task_authorization_does_not_hydrate_task_capsules() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("authorize", "src"))
        .await
        .expect("assignment creates");
    let capsule_dir = fixture
        .state
        .codex_home()
        .join("agent-task-coordination")
        .join("task_capsules");
    std::fs::create_dir_all(&capsule_dir).expect("capsule directory creates");
    std::fs::write(
        capsule_dir.join(format!("{}.json", assignment.assignment_id)),
        "{not-json",
    )
    .expect("corrupt capsule writes");

    assert!(
        fixture
            .store
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .is_err(),
        "the full task projection should hydrate and reject the corrupt capsule"
    );
    let authorization = fixture
        .store
        .get_agent_task_authorization(assignment.assignment_id)
        .await
        .expect("authorization projection reads without capsule hydration");

    assert_eq!(authorization.admission_origin, assignment.admission_origin);
    assert_eq!(authorization.current_attempt, attempt);
}


#[tokio::test]
async fn audit_mutation_recovery_f064_summary_records_configured_policy() {
    let fixture = Fixture::new().await;
    let (assignment, _) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("audit-policy", "src"))
        .await
        .expect("assignment creates");
    let now = Utc::now() + Duration::seconds(300);
    let recovery = crate::local::with_test_comparison_now(
        now,
        fixture.store.recover_nonproductive_assignment(
            assignment.assignment_id,
            now - Duration::seconds(37),
        ),
    )
    .await
    .expect("recovery evaluates");
    let NonproductiveRecovery::Recovered {
        receipt,
        productivity,
    } = recovery
    else {
        panic!("recovery should complete")
    };
    assert!(receipt.summary.contains("37 seconds"));
    assert_eq!(productivity.recovery_threshold_seconds, 37);
    assert_eq!(
        productivity.recovery_policy_version,
        NONPRODUCTIVE_RECOVERY_POLICY_VERSION
    );
}

#[tokio::test]
async fn audit_mutation_recovery_f065_validation_leases_are_server_bounded() {
    let fixture = Fixture::new().await;
    initialize_validation_repository(fixture.repo.path());
    let command = "cargo test audit lease";
    let (_, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft("audit-lease", "src", command),
        )
        .await
        .expect("assignment creates");
    let now = fixed_time("2030-01-01T00:00:00Z");
    let call = crate::local::with_test_comparison_now(
        now,
        start_focused_validation_with_evidence(
            &fixture.store,
            attempt.attempt_id,
            "audit-lease-call",
            command,
            ValidationEvidence {
                lease_expires_at: Some(fixed_time("2099-01-01T00:00:00Z")),
                ..ValidationEvidence::default()
            },
        ),
    )
    .await;
    assert_eq!(
        call.evidence.lease_expires_at,
        Some(now + Duration::seconds(MAX_VALIDATION_LEASE_SECONDS))
    );
    crate::local::with_test_comparison_now(
        now + Duration::seconds(10),
        fixture
            .store
            .heartbeat_validation_call(call.call_id.clone(), fixed_time("2099-01-01T00:00:00Z")),
    )
    .await
    .expect("heartbeat succeeds");
    let refreshed = fixture
        .store
        .get_validation_call(call.call_id)
        .await
        .expect("call reads")
        .expect("call exists");
    assert_eq!(
        refreshed.evidence.lease_expires_at,
        Some(now + Duration::seconds(10 + MAX_VALIDATION_LEASE_SECONDS))
    );
}


fn audit_capsule(assignment: &Assignment, attempt: &Attempt) -> TaskCapsuleV1 {
    TaskCapsuleV1 {
        schema_version: 1,
        assignment_id: assignment.assignment_id,
        attempt_id: attempt.attempt_id,
        role: assignment.role,
        capability_profile: assignment.capability_profile,
        requirements: assignment.acceptance_criteria.clone(),
        objective: assignment.objective.clone(),
        read_scope: assignment.read_scope.clone(),
        write_scope: assignment.write_scope.clone(),
        stop_condition: assignment.stop_condition.clone(),
        dependencies: assignment.dependencies.clone(),
        risk_hints: assignment.risk_hints.clone(),
        contract_claims: assignment.contract_claims.clone(),
        workspace_strategy: Some(assignment.workspace_strategy),
        relation: assignment.relation.clone(),
        architecture_contract_ref: assignment.architecture_contract_ref.clone(),
        integration_plan: assignment.integration_plan,
        relevant_handles: Vec::new(),
        workspace_epoch: assignment.start_epoch,
        workspace_manifest_hash: "audit-manifest".into(),
        prohibited_changes: assignment.prohibited_changes.clone(),
        required_evidence: assignment.required_evidence.clone(),
    }
}

#[tokio::test]
async fn audit_mutation_recovery_f070_capsule_publication_reconciles_committed_stage() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("audit-capsule", "src"))
        .await
        .expect("assignment creates");
    let canonical =
        serde_json::to_string(&audit_capsule(&assignment, &attempt)).expect("capsule serializes");
    fixture
        .store
        .attach_task_capsule(
            assignment.assignment_id,
            attempt.attempt_id,
            canonical.clone(),
        )
        .await
        .expect("capsule attaches");
    let dir = fixture
        .state
        .codex_home()
        .join("agent-task-coordination")
        .join("task_capsules");
    let final_path = dir.join(format!("{}.json", assignment.assignment_id));
    let stage_path = dir.join(format!(".{}.staged.json", assignment.assignment_id));
    fixture.store.close().await;
    std::fs::rename(&final_path, &stage_path).expect("crash stage simulates");
    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("store restarts");
    assert!(final_path.exists());
    assert!(!stage_path.exists());
    assert_eq!(
        restarted
            .get_agent_task(assignment.assignment_id, None)
            .await
            .expect("task reads")
            .assignment
            .task_capsule
            .as_deref(),
        Some(canonical.as_str())
    );
    restarted.close().await;
}

#[tokio::test]
async fn audit_capsule_cancelled_publication_serializes_competing_attach_and_recovery() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("capsule-race", "src"))
        .await
        .expect("assignment");
    let canonical = serde_json::to_string(&audit_capsule(&assignment, &attempt)).expect("capsule");
    let pause = Arc::new(TestSnapshotCapturePause::new());
    let writer_store = fixture.store.clone();
    let writer_pause = pause.clone();
    let payload = canonical.clone();
    let writer = tokio::spawn(async move {
        with_test_snapshot_capture_pause(
            writer_pause,
            writer_store.attach_task_capsule(assignment.assignment_id, attempt.attempt_id, payload),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), pause.started.acquire())
        .await
        .expect("publication starts")
        .expect("pause open")
        .forget();
    writer.abort();
    assert!(
        writer
            .await
            .expect_err("caller is cancelled")
            .is_cancelled()
    );
    let competing_store = fixture.store.clone();
    let payload = canonical.clone();
    let competitor = tokio::spawn(async move {
        competing_store
            .attach_task_capsule(assignment.assignment_id, attempt.attempt_id, payload)
            .await
    });
    let recovery = LocalAgentTaskStore::initialize(&fixture.state);
    tokio::pin!(recovery);
    let while_paused =
        tokio::time::timeout(std::time::Duration::from_millis(200), &mut recovery).await;
    pause.release.add_permits(1);
    assert!(
        while_paused.is_err(),
        "recovery must wait for capsule publication"
    );
    // Recovery may own the database lock, so keep polling it alongside the competitor.
    let (competing_result, recovered) = tokio::join!(competitor, recovery);
    assert!(
        matches!(competing_result.expect("competitor joins"), Err(StoreError::TaskCapsuleAlreadyAttached(id)) if id == assignment.assignment_id)
    );
    let recovered = recovered.expect("recovery succeeds");
    let task = recovered
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("capsule reloads");
    assert_eq!(
        task.assignment.task_capsule.as_deref(),
        Some(canonical.as_str())
    );
    recovered.close().await;
}


#[tokio::test]
async fn wake_wait_ends_when_store_closes() {
    let fixture = Fixture::new().await;
    let waiter = fixture
        .store
        .wait_for_wake_events("empty-root".into(), None);
    tokio::pin!(waiter);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut waiter)
            .await
            .is_err()
    );
    fixture.store.close().await;
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("shutdown wakes the waiter"),
        Err(StoreError::Sql(sqlx::Error::PoolClosed))
    ));
}

fn run_git(repo: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("Git command starts");
    assert!(
        output.status.success(),
        "Git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn initialize_validation_repository(repo: &std::path::Path) {
    std::fs::create_dir_all(repo.join("src")).expect("source directory creates");
    std::fs::write(repo.join("src/lib.rs"), "pub fn initial() {}\n")
        .expect("source fixture writes");
    std::fs::write(repo.join("README.md"), "initial documentation\n")
        .expect("documentation fixture writes");
    run_git(repo, &["init", "--quiet"]);
    run_git(repo, &["config", "user.email", "audit@example.invalid"]);
    run_git(repo, &["config", "user.name", "Audit Test"]);
    run_git(repo, &["add", "src/lib.rs", "README.md"]);
    run_git(repo, &["commit", "--quiet", "-m", "initial"]);
}

#[tokio::test]
async fn audit_validation_receipt_failed_and_cancelled_do_not_refresh_progress() {
    let fixture = Fixture::new().await;
    initialize_validation_repository(fixture.repo.path());
    let pool = coordination_pool(&fixture).await;
    let command = "focused proof";
    for (ordinal, status) in [
        ValidationCallStatus::Failed,
        ValidationCallStatus::Cancelled,
    ]
    .into_iter()
    .enumerate()
    {
        let (_, attempt) = fixture
            .store
            .create_assignment(
                fixture.repo.path(),
                validation_worker_draft(&format!("terminal-root-{ordinal}"), "src", command),
            )
            .await
            .expect("assignment creates");
        let mut call = start_focused_validation_with_evidence(
            &fixture.store,
            attempt.attempt_id,
            &format!("audit-validation-terminal-{ordinal}"),
            command,
            ValidationEvidence::default(),
        )
        .await;
        let prior = fixed_time("2020-01-01T00:00:00Z") + Duration::seconds(ordinal as i64);
        let prior_json = serde_json::to_string(&prior).expect("progress timestamp serializes");
        sqlx::query("UPDATE workspace_actors SET last_progress_at = ? WHERE attempt_id = ?")
            .bind(&prior_json)
            .bind(attempt.attempt_id.to_string())
            .execute(&pool)
            .await
            .expect("prior progress persists");
        call.status = status;
        call.recorded_at += Duration::milliseconds(1);
        fixture
            .store
            .record_validation_call(call)
            .await
            .expect("terminal validation records");
        let progress = sqlx::query_scalar::<_, String>(
            "SELECT last_progress_at FROM workspace_actors WHERE attempt_id = ?",
        )
        .bind(attempt.attempt_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("progress reads");
        assert_eq!(progress, prior_json);
    }
}






#[tokio::test]
async fn late_validation_settles_after_abandonment_without_creating_proof() {
    let fixture = Fixture::new().await;
    let command = "focused validation";
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft("late-validation", "src", command),
        )
        .await
        .unwrap();
    let call =
        start_focused_validation(&fixture.store, attempt.attempt_id, "late-call", command).await;
    fixture
        .store
        .abandon_agent_task(
            TaskActor::Root,
            assignment.assignment_id,
            "stop".to_string(),
        )
        .await
        .unwrap();
    let terminal = finish_focused_validation(&fixture.store, call).await;
    assert_eq!(terminal.status, ValidationCallStatus::Succeeded);
    assert_eq!(terminal.evidence.end_epoch, None);
    let task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .unwrap();
    assert_ne!(task.current_attempt.state, AttemptState::Active);
    assert!(task.current_attempt.sealed_at.is_some());
    let pool = coordination_pool(&fixture).await;
    let running: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM validation_calls WHERE call_id = 'late-call' AND status = '\"running\"'").fetch_one(&pool).await.unwrap();
    assert_eq!(running, 0);
}









#[tokio::test]
async fn audit_task_view_terminal_actor_with_future_expiry_is_released() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("task-view-lease-root", "src"),
        )
        .await
        .expect("assignment creates");
    let pool = coordination_pool(&fixture).await;
    let future = Utc::now() + Duration::hours(1);
    sqlx::query(
        "UPDATE workspace_actors SET state = 'terminal', lease_expires_at = ? WHERE attempt_id = ?",
    )
    .bind(serde_json::to_string(&future).expect("future expiry serializes"))
    .bind(attempt.attempt_id.to_string())
    .execute(&pool)
    .await
    .expect("persisted actor state changes");

    let task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("task view reads");
    assert_eq!(
        task.workspace_status.lease_state,
        Some(LeaseState::Released)
    );
}

#[tokio::test]
async fn audit_task_view_validation_history_and_receipt_references_are_complete() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("task-view-validation-root", "src"),
        )
        .await
        .expect("assignment creates");
    let pool = coordination_pool(&fixture).await;
    let recorded_at = Utc::now();
    let mut transaction = pool.begin().await.expect("validation transaction begins");
    for index in 0..=MAX_VALIDATION_CALLS_PER_TASK {
        let call = ValidationCall {
            call_id: format!("audit-call-{index:03}"),
            attempt_id: attempt.attempt_id,
            command_summary: format!("audit validation {index}"),
            evidence: ValidationEvidence::default(),
            status: ValidationCallStatus::Succeeded,
            recorded_at: recorded_at + Duration::seconds((index % 3) as i64),
        };
        sqlx::query(
            "INSERT INTO validation_calls (call_id, attempt_id, body_json, status, recorded_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&call.call_id)
        .bind(attempt.attempt_id.to_string())
        .bind(serde_json::to_string(&call).expect("validation call serializes"))
        .bind(serde_json::to_string(&call.status).expect("validation status serializes"))
        .bind(serde_json::to_string(&call.recorded_at).expect("validation time serializes"))
        .execute(&mut *transaction)
        .await
        .expect("validation call inserts");
    }
    let receipt = AgentReceipt {
        assignment_id: assignment.assignment_id,
        attempt_id: attempt.attempt_id,
        status: AgentStatusClaim::NeedsMain,
        summary: "receipt references the oldest retained call".to_string(),
        criterion_results: vec![CriterionResult {
            evidence_ref: None,
            criterion_id: criterion().id,
            status: CriterionStatus::NotRun,
            evidence: None,
        }],
        declared_changes: Vec::new(),
        validation_call_ids: vec!["audit-call-000".to_string()],
        blockers: vec!["task view completeness fixture".to_string()],
        risks: Vec::new(),
        next_action: Some("inspect complete task view".to_string()),
        architecture_contract: None,
        evidence_epoch: 0,
        sealed_at: recorded_at,
    };
    sqlx::query(
        "INSERT INTO receipts (attempt_id, assignment_id, status, body_json, sealed_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(attempt.attempt_id.to_string())
    .bind(assignment.assignment_id.to_string())
    .bind(serde_json::to_string(&receipt.status).expect("receipt status serializes"))
    .bind(serde_json::to_string(&receipt).expect("receipt serializes"))
    .bind(serde_json::to_string(&receipt.sealed_at).expect("receipt time serializes"))
    .execute(&mut *transaction)
    .await
    .expect("receipt inserts");
    transaction.commit().await.expect("task fixture commits");

    let task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("task view reads");
    assert_eq!(
        task.validation_calls.len(),
        MAX_VALIDATION_CALLS_PER_TASK + 1
    );
    assert!(task.validation_calls.windows(2).all(|pair| {
        (pair[0].recorded_at, &pair[0].call_id) <= (pair[1].recorded_at, &pair[1].call_id)
    }));
    let receipt = task.receipt.expect("receipt is present");
    assert!(receipt.validation_call_ids.iter().all(|receipt_id| {
        task.validation_calls
            .iter()
            .any(|call| &call.call_id == receipt_id)
    }));
}

#[tokio::test]
async fn audit_task_view_wake_read_distinguishes_no_stream_from_empty() {
    let fixture = Fixture::new().await;
    let no_stream = fixture
        .store
        .read_wake_events("missing-wake-root".to_string(), None)
        .await
        .expect("missing stream reads");
    assert_eq!(no_stream.status, WakeReadStatus::NoStream);
    assert!(!no_stream.timed_out);

    fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("healthy-wake-root", "src"),
        )
        .await
        .expect("wake-producing assignment creates");
    let available = fixture
        .store
        .read_wake_events("healthy-wake-root".to_string(), None)
        .await
        .expect("wake events read");
    assert_eq!(available.status, WakeReadStatus::EventsAvailable);
    let cursor = available.latest_event_id.expect("wake cursor exists");
    let empty = fixture
        .store
        .read_wake_events("healthy-wake-root".to_string(), Some(cursor))
        .await
        .expect("empty stream read succeeds");
    assert_eq!(empty.status, WakeReadStatus::Empty);
    assert!(!empty.timed_out);
}

#[tokio::test]
async fn audit_task_view_wake_checkpoint_rejects_foreign_event() {
    let fixture = Fixture::new().await;
    fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("checkpoint-owner-root", "src/owner"),
        )
        .await
        .expect("owner assignment creates");
    fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("checkpoint-foreign-root", "src/foreign"),
        )
        .await
        .expect("foreign assignment creates");
    let foreign = fixture
        .store
        .read_wake_events("checkpoint-foreign-root".to_string(), None)
        .await
        .expect("foreign wake reads")
        .latest_event_id
        .expect("foreign wake exists");
    let expected = fixture
        .store
        .automatic_wake_cursor(
            "checkpoint-owner-root".to_string(),
            "/root/consumer".to_string(),
        )
        .await
        .expect("owner cursor initializes");
    let error = fixture
        .store
        .compare_and_swap_automatic_wake_cursor(
            "checkpoint-owner-root".to_string(),
            "/root/consumer".to_string(),
            expected,
            foreign,
        )
        .await
        .expect_err("foreign event is rejected");
    assert!(matches!(error, StoreError::InvalidWakeWatermark(_)));
}

#[tokio::test]
async fn audit_task_view_wake_checkpoint_cannot_move_backward() {
    let fixture = Fixture::new().await;
    let (_, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("checkpoint-order-root", "src"),
        )
        .await
        .expect("assignment creates");
    fixture
        .store
        .append_observation(
            attempt.attempt_id,
            ObservationKind::Reading,
            "newer event".to_string(),
            None,
        )
        .await
        .expect("newer event appends");
    let events = fixture
        .store
        .read_wake_events("checkpoint-order-root".to_string(), None)
        .await
        .expect("wake events read")
        .updated_agents;
    let older = events.first().expect("older wake exists").event_id;
    let newer = events.last().expect("newer wake exists").event_id;
    let expected = fixture
        .store
        .automatic_wake_cursor(
            "checkpoint-order-root".to_string(),
            "/root/consumer".to_string(),
        )
        .await
        .expect("cursor initializes");
    assert!(
        fixture
            .store
            .compare_and_swap_automatic_wake_cursor(
                "checkpoint-order-root".to_string(),
                "/root/consumer".to_string(),
                expected,
                newer,
            )
            .await
            .expect("cursor advances")
    );
    let error = fixture
        .store
        .compare_and_swap_automatic_wake_cursor(
            "checkpoint-order-root".to_string(),
            "/root/consumer".to_string(),
            Some(newer),
            older,
        )
        .await
        .expect_err("cursor regression is rejected");
    assert!(matches!(error, StoreError::WakeWatermarkRegression { .. }));
}

struct Fixture {
    _codex_home: TempDir,
    repo: TempDir,
    state: Arc<StateRuntime>,
    store: LocalAgentTaskStore,
}

impl Fixture {
    async fn new() -> Self {
        let seed = std::env::var_os("KD4_AGENT_TASK_FIXTURE_SEED").map(std::path::PathBuf::from);
        Self::with_seed(seed.as_deref()).await
    }

    async fn with_seed(seed: Option<&std::path::Path>) -> Self {
        let codex_home = TempDir::new().expect("codex home tempdir");
        let repo = TempDir::new().expect("repository tempdir");
        if let Some(seed) = seed {
            copy_fixture_seed(seed, codex_home.path()).await;
        }
        let state =
            StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
                .await
                .expect("state runtime initializes");
        let store = LocalAgentTaskStore::initialize(&state)
            .await
            .expect("task store initializes");
        Self {
            _codex_home: codex_home,
            repo,
            state,
            store,
        }
    }
}

// The runner owns this directory for one invocation, including all nextest
// processes. Migration/recovery tests still initialize their own fresh stores.
async fn copy_fixture_seed(root: &std::path::Path, destination: &std::path::Path) {
    let lock_path = root.join("seed.lock");
    let _lock = tokio::task::spawn_blocking(move || {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .expect("open seed lock");
        lock.lock().expect("lock fixture seed");
        lock
    })
    .await
    .expect("seed lock worker");
    let ready = root.join("ready");
    let relative_paths = [
        std::path::PathBuf::from(codex_state::state_db_filename()),
        std::path::PathBuf::from(codex_state::logs_db_filename()),
        std::path::PathBuf::from(codex_state::goals_db_filename()),
        std::path::PathBuf::from("agent-task-coordination/agent_tasks.sqlite"),
    ];
    if !ready.exists() {
        let build = TempDir::new_in(root).expect("unpublished seed directory");
        let state = StateRuntime::init(build.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("seed state migrates");
        let store = LocalAgentTaskStore::initialize(&state)
            .await
            .expect("seed store migrates");
        store.close().await;
        state.close().await;
        // Closed pools must have checkpointed all data; never copy a live WAL.
        for relative in &relative_paths {
            let database = build.path().join(relative);
            assert!(database.is_file(), "missing seed database: {database:?}");
            let wal = std::path::PathBuf::from(format!("{}-wal", database.display()));
            assert!(
                wal.metadata().map_or(true, |metadata| metadata.len() == 0),
                "uncheckpointed seed WAL"
            );
        }
        std::fs::rename(build.path(), &ready).expect("publish complete seed");
    }
    for relative in relative_paths {
        let target = destination.join(&relative);
        std::fs::create_dir_all(target.parent().expect("database parent"))
            .expect("fixture directories");
        std::fs::copy(ready.join(relative), target).expect("copy closed seed database");
    }
}

#[tokio::test]
async fn migrated_fixture_seed_is_reused_without_sharing_mutable_databases() {
    let seed = TempDir::new().unwrap();
    let (first, second) = tokio::join!(
        Fixture::with_seed(Some(seed.path())),
        Fixture::with_seed(Some(seed.path())),
    );
    let first_pool = coordination_pool(&first).await;
    sqlx::query("CREATE TABLE fixture_isolation_probe (value INTEGER)")
        .execute(&first_pool)
        .await
        .unwrap();
    let seed_path = seed
        .path()
        .join("ready/agent-task-coordination/agent_tasks.sqlite");
    let before = std::fs::read(&seed_path).unwrap();
    let second_pool = coordination_pool(&second).await;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fixture_isolation_probe'",
    )
    .fetch_one(&second_pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
    let third = Fixture::with_seed(Some(seed.path())).await;
    assert_eq!(std::fs::read(seed_path).unwrap(), before);
    first_pool.close().await;
    second_pool.close().await;
    for fixture in [first, second, third] {
        fixture.store.close().await;
        fixture.state.close().await;
    }
}

fn fixed_time(value: &str) -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339(value)
        .expect("fixed timestamp parses")
        .with_timezone(&Utc)
}

fn json_time(value: &str) -> String {
    serde_json::to_string(value).expect("fixed timestamp serializes")
}

async fn coordination_pool(fixture: &Fixture) -> sqlx::SqlitePool {
    let database_path = fixture
        .state
        .codex_home()
        .join("agent-task-coordination")
        .join("agent_tasks.sqlite");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(database_path)
        .foreign_keys(true);
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("coordination database opens")
}



async fn wait_out_transient_writer_contention<T>(
    blocker_pool: &sqlx::SqlitePool,
    write: impl std::future::Future<Output = StoreResult<T>>,
) -> T {
    let mut blocker = blocker_pool
        .acquire()
        .await
        .expect("coordination connection opens");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *blocker)
        .await
        .expect("writer lock is acquired");
    tokio::pin!(write);
    if let Ok(early) = tokio::time::timeout(std::time::Duration::from_millis(50), &mut write).await
    {
        panic!(
            "the write must wait for the writer lock instead of completing early: {:?}",
            early.err()
        );
    }
    sqlx::query("ROLLBACK")
        .execute(&mut *blocker)
        .await
        .expect("writer lock is released");
    tokio::time::timeout(std::time::Duration::from_secs(1), write)
        .await
        .expect("the write resumes promptly after the writer lock is released")
        .expect("the write survives transient writer contention")
}

#[tokio::test]
async fn lease_heartbeats_wait_for_transient_writer_contention() {
    let fixture = Fixture::new().await;
    let root_session_id = "contended-lease-root";
    let command = "cargo test -p contended lease";
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft(root_session_id, "src", command),
        )
        .await
        .expect("worker assignment");
    let binding = bind_test_agent(
        &fixture.store,
        assignment.assignment_id,
        attempt.attempt_id,
        root_session_id,
    )
    .await;
    let call = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "contended-lease-call",
        command,
    )
    .await;
    let blocker_pool = coordination_pool(&fixture).await;

    // Each operation reads before it writes; a deferred read snapshot cannot wait
    // for the writer, so these must reserve it up front like other lease writes.
    assert!(
        wait_out_transient_writer_contention(
            &blocker_pool,
            fixture
                .store
                .heartbeat_typed_workspace_actor(binding, /*progress*/ false),
        )
        .await,
        "the typed actor lease renews after contention"
    );
    assert!(
        wait_out_transient_writer_contention(
            &blocker_pool,
            fixture
                .store
                .heartbeat_validation_call(call.call_id.clone(), Utc::now()),
        )
        .await,
        "the running validation lease renews after contention"
    );
    blocker_pool.close().await;
}

#[tokio::test]
async fn migration_reopens_store_with_retired_capture_generation() {
    let fixture = Fixture::new().await;
    let (assignment, _) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("resume-migration", "src"))
        .await
        .expect("assignment creates");
    fixture.store.close().await;
    let pool = coordination_pool(&fixture).await;

    // Model the released migration independently of the current embedded set.
    // Retiring its consumer must not remove this version or change its checksum.
    let mut historical_migrations = task_store_migrator_through(21).migrations.into_owned();
    historical_migrations.push(sqlx::migrate::Migration::new(
        22,
        Cow::Borrowed("workspace capture generation"),
        sqlx::migrate::MigrationType::Simple,
        sqlx::SqlSafeStr::into_sql_str(
            "ALTER TABLE workspace_repositories\n\
ADD COLUMN capture_generation INTEGER NOT NULL DEFAULT 0 CHECK (capture_generation >= 0);\n",
        ),
        false,
    ));
    // Verify the historical checksum without omitting later applied migrations.
    historical_migrations.extend(
        TEST_MIGRATOR
            .migrations
            .iter()
            .filter(|migration| migration.version > 22)
            .cloned(),
    );
    sqlx::migrate::Migrator::with_migrations(historical_migrations)
        .run(&pool)
        .await
        .expect("historical migration applies or matches its persisted checksum");
    sqlx::query("UPDATE workspace_repositories SET capture_generation = 7")
        .execute(&pool)
        .await
        .expect("historical generation persists");

    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("store reopens with migration 22 already applied");
    assert_eq!(
        restarted
            .get_agent_task(assignment.assignment_id, None)
            .await
            .expect("existing task remains readable")
            .assignment,
        assignment
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT capture_generation FROM workspace_repositories")
            .fetch_one(&pool)
            .await
            .expect("historical generation reads"),
        7
    );
    restarted
        .create_assignment(fixture.repo.path(), worker_draft("after-resume", "docs"))
        .await
        .expect("resumed store remains writable");
    restarted.close().await;
    pool.close().await;
}

#[tokio::test]
async fn validation_attempt_index_upgrades_existing_history_without_changing_results() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("memory database opens");
    task_store_migrator_through(22)
        .run(&pool)
        .await
        .expect("prior schema applies");
    sqlx::raw_sql(
        "INSERT INTO assignments VALUES ('assignment', 'root', '{}', '\"2026-01-01T00:00:00Z\"');
         INSERT INTO attempts VALUES ('target', 'assignment', 0, NULL, '\"active\"', '\"2026-01-01T00:00:00Z\"', NULL);
         INSERT INTO attempts VALUES ('other', 'assignment', 1, NULL, '\"active\"', '\"2026-01-01T00:00:00Z\"', NULL);
         INSERT INTO validation_calls VALUES
             ('z-first', 'target', 'first', '\"succeeded\"', '\"2026-01-01T00:00:00Z\"'),
             ('b-tied', 'target', 'third', '\"running\"', '\"2026-01-01T00:00:01Z\"'),
             ('a-tied', 'target', 'second', '\"running\"', '\"2026-01-01T01:00:01+01:00\"'),
             ('unrelated', 'other', 'not returned', '\"running\"', '\"2026-01-01T00:00:00Z\"');",
    )
    .execute(&pool)
    .await
    .expect("existing history inserts");
    let queries = [
        (
            "SELECT body_json FROM validation_calls WHERE attempt_id = ? ORDER BY julianday(json_extract(recorded_at, '$')), call_id",
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string(),
            ],
        ),
        (
            "SELECT call_id FROM validation_calls WHERE attempt_id = ? AND status = '\"running\"' ORDER BY call_id",
            vec!["a-tied".to_string(), "b-tied".to_string()],
        ),
    ];
    for upgraded in [false, true] {
        if upgraded {
            TEST_MIGRATOR.run(&pool).await.expect("history upgrades");
        }
        for (query, expected) in &queries {
            let actual = sqlx::query_scalar::<_, String>(*query)
                .bind("target")
                .fetch_all(&pool)
                .await
                .expect("history reads");
            assert_eq!(&actual, expected);
            let mut plan_query = sqlx::QueryBuilder::<sqlx::Sqlite>::new("EXPLAIN QUERY PLAN ");
            plan_query.push(*query);
            let plan = plan_query
                .build_query_as::<(i64, i64, i64, String)>()
                .bind("target")
                .fetch_all(&pool)
                .await
                .expect("query plan reads");
            assert_eq!(
                plan.iter().any(|(_, _, _, detail)| {
                    detail.contains(
                        "SEARCH validation_calls USING INDEX validation_calls_attempt_idx",
                    )
                }),
                upgraded
            );
        }
    }
    pool.close().await;
}

#[tokio::test]
async fn upgrade_reclaims_retired_pages_and_drops_duplicate_wake_indexes() {
    let codex_home = TempDir::new().expect("codex home tempdir");
    let repo = TempDir::new().expect("repository tempdir");
    let state = StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
        .await
        .expect("state runtime initializes");
    let coordination_root = state.codex_home().join("agent-task-coordination");
    tokio::fs::create_dir_all(&coordination_root)
        .await
        .expect("coordination directory creates");
    let predecessor_pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(coordination_root.join("agent_tasks.sqlite"))
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .expect("predecessor database opens");
    task_store_migrator_through(20)
        .run(&predecessor_pool)
        .await
        .expect("prior schema applies");
    // Dropped tables leave their pages on the freelist while auto_vacuum is off.
    sqlx::raw_sql(
        "CREATE TABLE retired_payloads (payload BLOB NOT NULL);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 64)
         INSERT INTO retired_payloads SELECT zeroblob(65536) FROM n;
         DROP TABLE retired_payloads;",
    )
    .execute(&predecessor_pool)
    .await
    .expect("retired payload pages are freed");
    let retired_pages = sqlx::query_scalar::<_, i64>("PRAGMA freelist_count")
        .fetch_one(&predecessor_pool)
        .await
        .expect("predecessor freelist reads");
    assert!(retired_pages > 0, "the predecessor retains freed pages");
    predecessor_pool.close().await;

    let store = LocalAgentTaskStore::initialize(&state)
        .await
        .expect("production initializer upgrades the predecessor database");
    let fixture = Fixture {
        _codex_home: codex_home,
        repo,
        state,
        store,
    };
    let pool = coordination_pool(&fixture).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA freelist_count")
            .fetch_one(&pool)
            .await
            .expect("upgraded freelist reads"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT name FROM sqlite_master
             WHERE type = 'index'
               AND name IN ('observations_wake_idx', 'wake_events_root_sequence_idx')",
        )
        .fetch_all(&pool)
        .await
        .expect("index names read"),
        Vec::<String>::new(),
        "unique constraint indexes already cover (root_session_id, wake_sequence)"
    );
    pool.close().await;
    fixture.store.close().await;
}

#[tokio::test]
async fn migration_removes_workspace_mutation_blocking_schema() {
    let codex_home = TempDir::new().expect("codex home tempdir");
    let repo = TempDir::new().expect("repository tempdir");
    let state = StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
        .await
        .expect("state runtime initializes");
    let coordination_root = state.codex_home().join("agent-task-coordination");
    tokio::fs::create_dir_all(&coordination_root)
        .await
        .expect("coordination directory creates");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(coordination_root.join("agent_tasks.sqlite"))
        .create_if_missing(true)
        .foreign_keys(true);
    let predecessor_pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("predecessor database opens");
    task_store_migrator_through(15)
        .run(&predecessor_pool)
        .await
        .expect("migrations through 0015 apply");

    let repository =
        crate::scope::repository_identity(repo.path()).expect("repository identity resolves");
    let created_at = json_time("2026-08-24T12:00:00Z");
    sqlx::query(
        "INSERT INTO workspace_repositories (
            workspace_id, repository_id, canonical_root, epoch, updated_at
         ) VALUES (?, ?, ?, 1, ?)",
    )
    .bind(&repository.workspace_id)
    .bind(&repository.id)
    .bind(&repository.canonical_path)
    .bind(&created_at)
    .execute(&predecessor_pool)
    .await
    .expect("retained repository row seeds");
    sqlx::query(
        "INSERT INTO workspace_events (
            workspace_id, epoch, actor_id, actor_kind, attribution_confidence,
            paths_json, contracts_json, created_at
         ) VALUES (?, 1, 'legacy-root', ?, ?, ?, ?, ?)",
    )
    .bind(&repository.workspace_id)
    .bind(serde_json::to_string(&WorkspaceActorKind::Root).expect("actor kind serializes"))
    .bind(
        serde_json::to_string(&AttributionConfidence::Definitive).expect("attribution serializes"),
    )
    .bind(r#"["src/lib.rs"]"#)
    .bind(r#"["stable-contract"]"#)
    .bind(&created_at)
    .execute(&predecessor_pool)
    .await
    .expect("retained event seeds");
    sqlx::query(
        "INSERT INTO actor_supporting_reads (
            workspace_id, actor_id, path, manifest_entry_json, read_epoch, read_at
         ) VALUES (?, 'legacy-root', 'src/lib.rs', '{}', 1, ?)",
    )
    .bind(&repository.workspace_id)
    .bind(&created_at)
    .execute(&predecessor_pool)
    .await
    .expect("retired supporting read seeds");
    sqlx::query(
        "INSERT INTO workspace_manifest_payloads (
            workspace_id, manifest_id, payload_format_version,
            canonical_manifest_bytes, entry_count, payload_byte_count, created_at
         ) VALUES (?, 'legacy-manifest', 1, X'00', 0, 1, ?)",
    )
    .bind(&repository.workspace_id)
    .bind(&created_at)
    .execute(&predecessor_pool)
    .await
    .expect("retired manifest payload seeds");
    sqlx::query(
        "INSERT INTO workspace_mutation_leases (
            lease_id, workspace_id, root_session_id, actor_id, attempt_id,
            start_epoch, paths_json, contracts_json, expected_manifest_json,
            state, created_at, heartbeat_at, expires_at, released_at, actor_kind
         ) VALUES (
            'legacy-lease', ?, 'legacy-root', 'legacy-root', NULL,
            1, '[]', '[]', '[]', 'released', ?, ?, ?, ?, ?
         )",
    )
    .bind(&repository.workspace_id)
    .bind(&created_at)
    .bind(&created_at)
    .bind(&created_at)
    .bind(&created_at)
    .bind(serde_json::to_string(&WorkspaceActorKind::Root).expect("actor kind serializes"))
    .execute(&predecessor_pool)
    .await
    .expect("retired mutation lease seeds");
    sqlx::query(
        "INSERT INTO workspace_finalization_fences (
            fence_id, workspace_id, root_session_id, state, created_at,
            expires_at, released_at
         ) VALUES ('legacy-fence', ?, 'legacy-root', 'released', ?, ?, ?)",
    )
    .bind(&repository.workspace_id)
    .bind(&created_at)
    .bind(&created_at)
    .bind(&created_at)
    .execute(&predecessor_pool)
    .await
    .expect("retired finalization fence seeds");
    predecessor_pool.close().await;

    let store = LocalAgentTaskStore::initialize(&state)
        .await
        .expect("production initializer upgrades predecessor database");
    let fixture = Fixture {
        _codex_home: codex_home,
        repo,
        state,
        store,
    };
    let pool = coordination_pool(&fixture).await;
    let event = sqlx::query_as::<_, (String, String)>(
        "SELECT paths_json, contracts_json FROM workspace_events",
    )
    .fetch_one(&pool)
    .await
    .expect("historical workspace event is preserved");
    assert_eq!(event, (r#"["src/lib.rs"]"#.to_string(), r#"["stable-contract"]"#.to_string()));
    let remaining = sqlx::query_as::<_, (String, String)>(
        "SELECT type, name
         FROM sqlite_master
         WHERE (type = 'table' AND name IN (
                    'actor_supporting_reads',
                    'workspace_manifest_payloads',
                    'workspace_mutation_leases',
                    'workspace_finalization_fences',
                    'validation_evidence_revisions'
                ))
            OR (type = 'trigger' AND name LIKE 'finalization_blocks_%')
            OR (type = 'trigger' AND name LIKE 'validation_evidence_revision_%')
         ORDER BY type, name",
    )
    .fetch_all(&pool)
    .await
    .expect("post-migration schema reads");
    assert_eq!(remaining, Vec::<(String, String)>::new());
    pool.close().await;
    fixture.store.close().await;
}

async fn expire_workspace_actor_leases(
    fixture: &Fixture,
    attempt_ids: &[AttemptId],
) -> chrono::DateTime<Utc> {
    assert!(
        !attempt_ids.is_empty(),
        "at least one actor lease is required"
    );
    let database_path = fixture
        .state
        .codex_home()
        .join("agent-task-coordination")
        .join("agent_tasks.sqlite");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(database_path)
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("coordination database opens");
    let encoded_comparison_now = sqlx::query_scalar::<_, String>(
        "SELECT last_progress_at FROM workspace_actors WHERE attempt_id = ?",
    )
    .bind(attempt_ids[0].to_string())
    .fetch_one(&pool)
    .await
    .expect("workspace actor comparison time reads");
    let comparison_now: chrono::DateTime<Utc> = serde_json::from_str(&encoded_comparison_now)
        .expect("workspace actor comparison time decodes");
    let stale_at = comparison_now - Duration::seconds(DEFAULT_WORKSPACE_LEASE_SECONDS + 1);
    let encoded_stale_at = serde_json::to_string(&stale_at).expect("stale time serializes");
    for attempt_id in attempt_ids {
        let updated = sqlx::query(
            "UPDATE workspace_actors
             SET state = 'active', last_progress_at = ?, lease_expires_at = ?
             WHERE attempt_id = ?",
        )
        .bind(&encoded_stale_at)
        .bind(&encoded_stale_at)
        .bind(attempt_id.to_string())
        .execute(&pool)
        .await
        .expect("workspace actor lease expires");
        assert_eq!(updated.rows_affected(), 1);
    }
    pool.close().await;
    comparison_now
}

fn criterion() -> AcceptanceCriterion {
    AcceptanceCriterion {
        id: "criterion-1".to_string(),
        text: "the requested behavior is proven".to_string(),
    }
}

fn worker_draft(root_session_id: &str, scope: &str) -> AssignmentDraft {
    AssignmentDraft {
        root_session_id: root_session_id.to_string(),
        admission_origin: AssignmentAdmissionOrigin::Typed,
        role: AgentRole::Worker,
        capability_profile: CapabilityProfile::ScopedSourceWrite,
        objective: "implement the bounded change".to_string(),
        acceptance_criteria: vec![criterion()],
        read_scope: Vec::new(),
        write_scope: vec![RepoScope {
            path: scope.to_string(),
            recursive: true,
        }],
        stop_condition: "stop after focused validation".to_string(),
        dependencies: Vec::new(),
        risk_hints: Vec::new(),
        required_evidence: Vec::new(),
        prohibited_changes: Vec::new(),
        contract_claims: Vec::new(),
        workspace_strategy: WorkspaceStrategy::Auto,
        relation: None,
        architecture_contract_ref: None,
    }
}

fn completed_receipt(validation_call_ids: Vec<String>) -> ReceiptDraft {
    ReceiptDraft {
        status: AgentStatusClaim::Completed,
        summary: "completed and validated".to_string(),
        criterion_results: vec![CriterionResult {
            evidence_ref: None,
            criterion_id: criterion().id,
            status: CriterionStatus::Passed,
            evidence: Some("focused validation passed".to_string()),
        }],
        declared_changes: Vec::new(),
        validation_call_ids,
        blockers: Vec::new(),
        risks: Vec::new(),
        next_action: None,
        architecture_contract: None,
    }
}

fn architecture_contract_for_worker(scope: &str) -> ArchitectureContractV1 {
    ArchitectureContractV1 {
        schema_version: ARCHITECTURE_CONTRACT_V1_SCHEMA_VERSION,
        objective: "implement the bounded change".to_string(),
        acceptance_criteria: vec![criterion()],
        read_scope: Vec::new(),
        write_scope: vec![RepoScope {
            path: scope.to_string(),
            recursive: true,
        }],
        stop_condition: "stop after focused validation".to_string(),
        risk_hints: Vec::new(),
        required_evidence: Vec::new(),
        prohibited_changes: Vec::new(),
        contract_claims: Vec::new(),
    }
}

#[tokio::test]
async fn architect_receipt_seals_canonical_contract_and_admits_exact_worker_projection() {
    use sha2::Digest;

    let fixture = Fixture::new().await;
    let architect_draft = AssignmentDraft {
        root_session_id: "architecture-root".to_string(),
        admission_origin: AssignmentAdmissionOrigin::Typed,
        role: AgentRole::Architect,
        capability_profile: CapabilityProfile::ReadSearch,
        objective: "define the worker contract".to_string(),
        acceptance_criteria: vec![AcceptanceCriterion {
            id: "architecture".to_string(),
            text: "seal one canonical worker contract".to_string(),
        }],
        read_scope: vec![RepoScope {
            path: "src".to_string(),
            recursive: true,
        }],
        write_scope: Vec::new(),
        stop_condition: "stop after sealing the contract".to_string(),
        dependencies: Vec::new(),
        risk_hints: Vec::new(),
        required_evidence: Vec::new(),
        prohibited_changes: Vec::new(),
        contract_claims: Vec::new(),
        workspace_strategy: WorkspaceStrategy::Shared,
        relation: None,
        architecture_contract_ref: None,
    };
    let (architect, architect_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), architect_draft)
        .await
        .expect("architect assignment");
    let expected_contract = architecture_contract_for_worker("src");
    let mut unnormalized = expected_contract.clone();
    unnormalized.objective = format!("  {}\n", unnormalized.objective);
    unnormalized.stop_condition = format!(" {} ", unnormalized.stop_condition);
    unnormalized.acceptance_criteria[0].id = " criterion-1 ".to_string();
    let receipt = fixture
        .store
        .submit_agent_receipt(
            architect_attempt.attempt_id,
            ReceiptDraft {
                status: AgentStatusClaim::Completed,
                summary: "architecture sealed".to_string(),
                criterion_results: vec![CriterionResult {
                    evidence_ref: None,
                    criterion_id: "architecture".to_string(),
                    status: CriterionStatus::Passed,
                    evidence: Some("canonical contract attached".to_string()),
                }],
                declared_changes: Vec::new(),
                validation_call_ids: Vec::new(),
                blockers: Vec::new(),
                risks: Vec::new(),
                next_action: None,
                architecture_contract: Some(unnormalized),
            },
        )
        .await
        .expect("architect receipt");
    let sealed = receipt
        .architecture_contract
        .expect("sealed architecture contract");
    assert_eq!(sealed.contract, expected_contract);
    assert_eq!(
        sealed.contract_sha256,
        format!("{:x}", sha2::Sha256::digest(serde_json::to_vec(&expected_contract).unwrap()))
    );

    let mut worker = worker_draft("architecture-root", "src");
    worker.dependencies = vec![architect.assignment_id];
    worker.architecture_contract_ref = Some(ArchitectureContractRef {
        architect_assignment_id: architect.assignment_id,
        architect_attempt_id: architect_attempt.attempt_id,
        contract_version: sealed.contract.schema_version,
        contract_sha256: sealed.contract_sha256,
    });
    fixture
        .store
        .create_assignment(fixture.repo.path(), worker)
        .await
        .expect("exact worker projection is admitted");
}

#[tokio::test]
async fn architect_dependent_workers_fail_closed_on_missing_or_wrong_contract_references() {
    let fixture = Fixture::new().await;
    let (architect, architect_attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            AssignmentDraft {
                root_session_id: "architecture-root".to_string(),
                admission_origin: AssignmentAdmissionOrigin::Typed,
                role: AgentRole::Architect,
                capability_profile: CapabilityProfile::ReadSearch,
                objective: "define the worker contract".to_string(),
                acceptance_criteria: vec![AcceptanceCriterion {
                    id: "architecture".to_string(),
                    text: "seal one canonical worker contract".to_string(),
                }],
                read_scope: vec![RepoScope {
                    path: "src".to_string(),
                    recursive: true,
                }],
                write_scope: Vec::new(),
                stop_condition: "stop after sealing the contract".to_string(),
                dependencies: Vec::new(),
                risk_hints: Vec::new(),
                required_evidence: Vec::new(),
                prohibited_changes: Vec::new(),
                contract_claims: Vec::new(),
                workspace_strategy: WorkspaceStrategy::Shared,
                relation: None,
                architecture_contract_ref: None,
            },
        )
        .await
        .expect("architect assignment");
    let sealed = fixture
        .store
        .submit_agent_receipt(
            architect_attempt.attempt_id,
            ReceiptDraft {
                status: AgentStatusClaim::Completed,
                summary: "architecture sealed".to_string(),
                criterion_results: vec![CriterionResult {
                    evidence_ref: None,
                    criterion_id: "architecture".to_string(),
                    status: CriterionStatus::Passed,
                    evidence: Some("canonical contract attached".to_string()),
                }],
                declared_changes: Vec::new(),
                validation_call_ids: Vec::new(),
                blockers: Vec::new(),
                risks: Vec::new(),
                next_action: None,
                architecture_contract: Some(architecture_contract_for_worker("src")),
            },
        )
        .await
        .expect("architect receipt")
        .architecture_contract
        .expect("sealed contract");

    let mut missing = worker_draft("architecture-root", "src");
    missing.dependencies = vec![architect.assignment_id];
    let error = fixture
        .store
        .create_assignment(fixture.repo.path(), missing)
        .await
        .expect_err("architect-dependent worker requires a reference");
    assert!(
        error
            .to_string()
            .contains("missing its architecture contract reference")
    );

    let mut wrong_hash = worker_draft("architecture-root", "src");
    wrong_hash.dependencies = vec![architect.assignment_id];
    wrong_hash.architecture_contract_ref = Some(ArchitectureContractRef {
        architect_assignment_id: architect.assignment_id,
        architect_attempt_id: architect_attempt.attempt_id,
        contract_version: sealed.contract.schema_version,
        contract_sha256: "0".repeat(64),
    });
    let error = fixture
        .store
        .create_assignment(fixture.repo.path(), wrong_hash)
        .await
        .expect_err("wrong contract hash fails closed");
    assert!(error.to_string().contains("version or hash does not match"));
}

#[tokio::test]
async fn explorer_cannot_seal_architecture_contract() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("architecture-root", "src");
    draft.role = AgentRole::Explorer;
    draft.capability_profile = CapabilityProfile::ReadSearch;
    draft.write_scope.clear();
    let (_assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("explorer assignment");
    let mut receipt = completed_receipt(Vec::new());
    receipt.architecture_contract = Some(architecture_contract_for_worker("src"));

    let error = fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, receipt)
        .await
        .expect_err("explorer cannot seal architecture");
    assert!(error.to_string().contains("only an Architect"));
}

fn validation_worker_draft(root_session_id: &str, scope: &str, command: &str) -> AssignmentDraft {
    let mut draft = worker_draft(root_session_id, scope);
    draft.required_evidence = vec![command.to_string()];
    draft
}

fn selective_worker_draft(
    root_session_id: &str,
    write_scope: &str,
    read_scope: &[&str],
) -> AssignmentDraft {
    let mut draft = validation_worker_draft(root_session_id, write_scope, "focused proof");
    draft.read_scope = read_scope
        .iter()
        .map(|path| RepoScope {
            path: (*path).to_string(),
            recursive: true,
        })
        .collect();
    draft
}

fn explorer_draft(root_session_id: &str, scope: &str, objective: &str) -> AssignmentDraft {
    AssignmentDraft {
        root_session_id: root_session_id.to_string(),
        admission_origin: AssignmentAdmissionOrigin::Typed,
        role: AgentRole::Explorer,
        capability_profile: CapabilityProfile::ReadSearch,
        objective: objective.to_string(),
        acceptance_criteria: vec![criterion()],
        read_scope: vec![RepoScope {
            path: scope.to_string(),
            recursive: true,
        }],
        write_scope: Vec::new(),
        stop_condition: "stop after recording the bounded finding".to_string(),
        dependencies: Vec::new(),
        risk_hints: Vec::new(),
        required_evidence: Vec::new(),
        prohibited_changes: Vec::new(),
        contract_claims: Vec::new(),
        workspace_strategy: WorkspaceStrategy::Auto,
        relation: None,
        architecture_contract_ref: None,
    }
}

async fn start_focused_validation(
    store: &LocalAgentTaskStore,
    attempt_id: AttemptId,
    call_id: &str,
    command: &str,
) -> ValidationCall {
    start_focused_validation_with_evidence(
        store,
        attempt_id,
        call_id,
        command,
        ValidationEvidence::default(),
    )
    .await
}

async fn start_focused_validation_with_evidence(
    store: &LocalAgentTaskStore,
    attempt_id: AttemptId,
    call_id: &str,
    command: &str,
    evidence: ValidationEvidence,
) -> ValidationCall {
    store
        .record_validation_call(ValidationCall {
            call_id: call_id.to_string(),
            attempt_id,
            command_summary: command.to_string(),
            evidence,
            status: ValidationCallStatus::Running,
            recorded_at: Utc::now(),
        })
        .await
        .expect("focused validation starts");
    store
        .get_validation_call(call_id.to_string())
        .await
        .expect("focused validation reads")
        .expect("focused validation exists")
}

async fn finish_focused_validation(
    store: &LocalAgentTaskStore,
    mut call: ValidationCall,
) -> ValidationCall {
    if call.evidence.validation_result.is_none() {
        call.evidence.validation_result = Some(serde_json::json!({
            "argv": [call.command_summary.clone()],
            "coveredPaths": ["."],
            "callId": call.call_id.clone(),
            "processId": null,
            "status": "succeeded",
            "durationMs": 1,
        }));
    }
    call.status = ValidationCallStatus::Succeeded;
    call.recorded_at += Duration::milliseconds(1);
    store
        .record_validation_call(call.clone())
        .await
        .expect("focused validation finishes");
    store
        .get_validation_call(call.call_id)
        .await
        .expect("finished validation reads")
        .expect("finished validation exists")
}

#[tokio::test]
async fn identical_validation_calls_record_independently_and_ignore_historical_coordination() {
    let fixture = Fixture::new().await;
    let command = "focused test";
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft("independent-validation-root", "src", command),
        )
        .await
        .expect("validation assignment creates");
    let first = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "independent-validation-first",
        command,
    )
    .await;

    let pool = coordination_pool(&fixture).await;
    let workspace_id = sqlx::query_scalar::<_, String>(
        "SELECT workspace_id FROM assignment_repositories WHERE assignment_id = ?",
    )
    .bind(assignment.assignment_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("workspace id reads");
    let now = serde_json::to_string(&Utc::now()).expect("time serializes");
    sqlx::query(
        "INSERT INTO validation_singleflight (
             workspace_id, start_epoch, fingerprint, leader_call_id, state,
             lease_expires_at, updated_at
         ) VALUES (?, ?, 'historical-fingerprint', ?, 'running', ?, ?)",
    )
    .bind(workspace_id)
    .bind(i64::try_from(first.evidence.start_epoch).expect("epoch fits SQLite"))
    .bind(&first.call_id)
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .expect("historical singleflight row seeds");
    sqlx::query(
        "INSERT INTO stale_recovery (
             attempt_id, stale_events, reconciliation_call_id, last_stale_epoch,
             last_reason, updated_at
         ) VALUES (?, 2, NULL, ?, 'historical stale state', ?)",
    )
    .bind(attempt.attempt_id.to_string())
    .bind(i64::try_from(first.evidence.start_epoch).expect("epoch fits SQLite"))
    .bind(&now)
    .execute(&pool)
    .await
    .expect("historical stale row seeds");
    pool.close().await;

    let second = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "independent-validation-second",
        command,
    )
    .await;
    let first = finish_focused_validation(&fixture.store, first).await;
    let second = finish_focused_validation(&fixture.store, second).await;
    assert_ne!(first.call_id, second.call_id);
    for call in [&first, &second] {
        assert_eq!(call.status, ValidationCallStatus::Succeeded);
        assert_eq!(
            call.evidence
                .validation_result
                .as_ref()
                .and_then(|result| result.get("callId"))
                .and_then(serde_json::Value::as_str),
            Some(call.call_id.as_str())
        );
    }
    let task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("task reads");
    assert!(task.workspace_status.stale_reason.is_none());
    assert_eq!(
        task.validation_calls
            .iter()
            .filter(|call| call.command_summary == command)
            .count(),
        2
    );
}

async fn controlled_write(
    store: &LocalAgentTaskStore,
    repo_root: &std::path::Path,
    root_session_id: &str,
    assignment_id: AssignmentId,
    attempt_id: AttemptId,
    path: &str,
    contents: &str,
) {
    bind_test_agent(store, assignment_id, attempt_id, root_session_id).await;
    std::fs::write(repo_root.join(path), contents).expect("controlled file write");
}

async fn bind_test_agent(
    store: &LocalAgentTaskStore,
    assignment_id: AssignmentId,
    attempt_id: AttemptId,
    root_session_id: &str,
) -> AgentTaskBinding {
    store
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id,
            attempt_id,
            agent_path: format!("/root/test-{attempt_id}"),
            task_name: format!("test-{attempt_id}"),
            thread_id: Some(format!("thread-{root_session_id}-{attempt_id}")),
        })
        .await
        .expect("test agent binds")
}

fn relation_draft(root_session_id: &str, role: AgentRole, target: AssignmentId) -> AssignmentDraft {
    let (capability_profile, kind) = match role {
        AgentRole::Reviewer => (CapabilityProfile::ReadSearchDiff, RelationKind::Review),
        AgentRole::Verifier => (
            CapabilityProfile::ReadSearchShell,
            RelationKind::Verification,
        ),
        _ => panic!("relation_draft supports only reviewer and verifier roles"),
    };
    AssignmentDraft {
        root_session_id: root_session_id.to_string(),
        admission_origin: AssignmentAdmissionOrigin::Typed,
        role,
        capability_profile,
        objective: format!("{role:?} the bounded change"),
        acceptance_criteria: vec![criterion()],
        read_scope: Vec::new(),
        write_scope: Vec::new(),
        stop_condition: "stop after an evidence-backed verdict".to_string(),
        dependencies: vec![target],
        risk_hints: Vec::new(),
        required_evidence: Vec::new(),
        prohibited_changes: Vec::new(),
        contract_claims: Vec::new(),
        workspace_strategy: WorkspaceStrategy::Auto,
        relation: Some(AssignmentRelation {
            kind,
            target_assignment_ids: vec![target],
        }),
        architecture_contract_ref: None,
    }
}

#[test]
fn ids_and_scope_validation_are_strict() {
    assert_eq!(AssignmentId::new().as_uuid().get_version_num(), 7);
    assert!(AssignmentId::try_from(Uuid::new_v4()).is_err());
    let mut bytes = *Uuid::now_v7().as_bytes();
    bytes[8] &= 0x3f;
    let non_rfc = Uuid::from_bytes(bytes);
    assert_eq!(non_rfc.get_version_num(), 7);
    assert!(AssignmentId::try_from(non_rfc).is_err());
    assert!(
        serde_json::from_value::<AssignmentId>(serde_json::json!(non_rfc.to_string())).is_err()
    );
    let repo = TempDir::new().expect("repository tempdir");
    assert!(
        normalize_repo_scopes(
            repo.path(),
            &[RepoScope {
                path: repo.path().display().to_string(),
                recursive: false,
            }]
        )
        .is_err()
    );
    assert!(
        normalize_repo_scopes(
            repo.path(),
            &[RepoScope {
                path: "../outside".to_string(),
                recursive: false,
            }]
        )
        .is_err()
    );
    assert!(
        normalize_repo_scopes(
            repo.path(),
            &[
                RepoScope {
                    path: "src".to_string(),
                    recursive: false,
                },
                RepoScope {
                    path: "src".to_string(),
                    recursive: true,
                },
            ]
        )
        .is_err()
    );
}

#[test]
fn reviewer_and_verifier_invariants_are_enforced() {
    let repo = TempDir::new().expect("repository tempdir");
    let target = AssignmentId::new();
    for role in [AgentRole::Reviewer, AgentRole::Verifier] {
        let valid = relation_draft("root", role, target);
        assert!(valid.clone().normalize(repo.path()).is_ok());
        let mut writes = valid.clone();
        writes.write_scope = vec![RepoScope { path: "src".into(), recursive: true }];
        assert!(matches!(
            writes.normalize(repo.path()),
            Err(StoreError::InvalidAssignment(message)) if message.contains("empty write scope")
        ));
        let mut missing_dependency = valid.clone();
        missing_dependency.dependencies.clear();
        let mut missing_relation = valid.clone();
        missing_relation.relation = None;
        let mut wrong_relation = valid.clone();
        wrong_relation.relation.as_mut().unwrap().kind = RelationKind::Integration;
        let mut extra_target = valid;
        extra_target.relation.as_mut().unwrap().target_assignment_ids.push(target);
        for invalid in [missing_dependency, missing_relation, wrong_relation, extra_target] {
            assert!(matches!(
                invalid.normalize(repo.path()),
                Err(StoreError::InvalidAssignment(message)) if message.contains("requires exactly one")
            ), "{role:?}");
        }
    }
}

#[tokio::test]
async fn selective_admission_uses_assignment_metadata_without_persisting_claims() {
    let fixture = Fixture::new().await;
    let root_session_id = "selective-overlap-root";
    let mut first_draft = selective_worker_draft(
        root_session_id,
        "src/first.rs",
        &["AGENTS.md", "src/types.rs"],
    );
    first_draft.contract_claims = vec!["shared-api".to_string()];
    let first = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), first_draft, true)
        .await
        .expect("first disjoint writer is admitted");
    assert_eq!(first.integration_plan, IntegrationPlan::SingleWriter);

    let mut second_draft = selective_worker_draft(
        root_session_id,
        "src/second.rs",
        &["AGENTS.md", "src/types.rs"],
    );
    second_draft.contract_claims = vec!["shared-api".to_string()];
    let second = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), second_draft, true)
        .await
        .expect("shared read scopes do not exclude a disjoint writer");
    assert_eq!(second.integration_plan, IntegrationPlan::RootOwned);
    assert_eq!(second.overlaps.benign_read_overlap_count, 1);

    let mut disjoint_draft = selective_worker_draft(root_session_id, "src/disjoint.rs", &[]);
    disjoint_draft.contract_claims = vec!["independent-api".to_string()];
    let disjoint = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), disjoint_draft, true)
        .await
        .expect("disjoint write and contract scopes remain single-writer work");
    assert_eq!(disjoint.integration_plan, IntegrationPlan::SingleWriter);

    let mut overlapping_draft = selective_worker_draft(root_session_id, "src", &["AGENTS.md"]);
    overlapping_draft.contract_claims = vec!["shared-api".to_string()];
    let overlapping = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), overlapping_draft, true)
        .await
        .expect("overlapping path and contract claims are admitted as metadata");
    assert_eq!(overlapping.integration_plan, IntegrationPlan::RootOwned);
    let pool = coordination_pool(&fixture).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM write_claims WHERE active = 1")
            .fetch_one(&pool)
            .await
            .expect("active write claim count reads"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM contract_claims WHERE active = 1 AND contract_name = 'shared-api'",
        )
        .fetch_one(&pool)
        .await
        .expect("active contract claim count reads"),
        0
    );
    pool.close().await;

    let sensitive = Fixture::new().await;
    sensitive
        .store
        .create_admitted_assignment(
            sensitive.repo.path(),
            explorer_draft(
                root_session_id,
                "src/critical.rs",
                "determine the critical invariant",
            ),
            true,
        )
        .await
        .expect("primary investigation is admitted");
    let read_overlap = sensitive
        .store
        .create_admitted_assignment(
            sensitive.repo.path(),
            selective_worker_draft(root_session_id, "src/critical.rs", &[]),
            true,
        )
        .await
        .expect("an active primary investigation is advisory to an overlapping writer");
    assert_eq!(read_overlap.integration_plan, IntegrationPlan::SingleWriter);
}

#[tokio::test]
async fn plain_message_admission_does_not_require_typed_proof_obligation() {
    let fixture = Fixture::new().await;
    let mut draft = selective_worker_draft("plain-admission-root", ".", &[]);
    draft.required_evidence.clear();
    draft.workspace_strategy = WorkspaceStrategy::Shared;
    assert!(matches!(
        fixture.store.create_admitted_assignment(fixture.repo.path(), draft.clone(), true).await,
        Err(StoreError::InvalidAssignment(message)) if message.contains("proof obligation")
    ));
    draft.admission_origin = AssignmentAdmissionOrigin::LegacyMessage {
        parent_assignment_id: None,
    };
    let admitted = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), draft, true)
        .await
        .expect("ordinary message admits without a typed proof obligation");
    assert!(admitted.assignment.required_evidence.is_empty());
    let receipt = fixture
        .store
        .record_legacy_agent_outcome(
            admitted.attempt.attempt_id,
            AgentStatusClaim::Completed,
            "ordinary final".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.status, AgentStatusClaim::Completed);
    assert!(receipt.validation_call_ids.is_empty());
    assert!(
        receipt
            .criterion_results
            .iter()
            .all(|result| result.status == CriterionStatus::NotRun)
    );
}

#[tokio::test]
async fn repository_root_scope_allows_nested_writer_metadata() {
    let fixture = Fixture::new().await;
    let root_session_id = "repository-root-overlap";
    let mut repository_wide = selective_worker_draft(root_session_id, ".", &[]);
    repository_wide.admission_origin = AssignmentAdmissionOrigin::LegacyMessage {
        parent_assignment_id: None,
    };
    let admitted = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), repository_wide, true)
        .await
        .expect("repository-wide legacy-compatible claim is admitted");
    assert_eq!(admitted.assignment.write_scope[0].path, ".");
    assert!(admitted.assignment.write_scope[0].covers_path("src/nested.rs"));

    let mut delegated_child = selective_worker_draft(root_session_id, ".", &[]);
    delegated_child.admission_origin = AssignmentAdmissionOrigin::LegacyMessage {
        parent_assignment_id: Some(admitted.assignment.assignment_id),
    };
    fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), delegated_child, true)
        .await
        .expect("an explicitly nested legacy claim may overlap its parent claim");

    let nested = fixture
        .store
        .create_admitted_assignment(
            fixture.repo.path(),
            selective_worker_draft(root_session_id, "src/nested.rs", &[]),
            true,
        )
        .await
        .expect("a nested writer may overlap the repository-wide claim");
    assert_eq!(nested.integration_plan, IntegrationPlan::RootOwned);
}

#[tokio::test]
async fn explorer_identity_rejects_only_the_same_primary_question() {
    let fixture = Fixture::new().await;
    let root_session_id = "explorer-identity-root";
    let first_draft = explorer_draft(
        root_session_id,
        "src/shared.rs",
        "trace the parser ownership",
    );
    let first = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), first_draft.clone(), true)
        .await
        .expect("first investigation is admitted");
    let duplicate = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), first_draft, true)
        .await
        .expect_err("the same canonical investigation is rejected");
    assert!(matches!(
        duplicate,
        StoreError::AdmissionRejected {
            reason: AdmissionRejectionReason::DuplicateExplorerInvestigation,
            reusable_assignment_id: Some(assignment_id),
        }
        if assignment_id == first.assignment.assignment_id
    ));

    let distinct = fixture
        .store
        .create_admitted_assignment(
            fixture.repo.path(),
            explorer_draft(
                root_session_id,
                "src/shared.rs",
                "trace the serializer ownership",
            ),
            true,
        )
        .await
        .expect("a distinct question may inspect the same surface");
    assert_eq!(distinct.overlaps.benign_read_overlap_count, 1);

    fixture
        .store
        .submit_agent_receipt(first.attempt.attempt_id, completed_receipt(Vec::new()))
        .await
        .expect("the first investigation result seals");
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("source directory");
    std::fs::write(fixture.repo.path().join("src/shared.rs"), "changed parser")
        .expect("workspace changes");
    let completed_duplicate = fixture
        .store
        .create_admitted_assignment(
            fixture.repo.path(),
            explorer_draft(
                root_session_id,
                "src/shared.rs",
                "trace the parser ownership",
            ),
            true,
        )
        .await
        .expect("a sealed investigation cannot prove freshness for new work");
    assert_ne!(
        completed_duplicate.assignment.assignment_id,
        first.assignment.assignment_id
    );
}

#[tokio::test]
async fn selective_multi_writer_admission_records_the_required_integration_plan() {
    let fixture = Fixture::new().await;
    let root_session_id = "integration-plan-root";
    let mut isolated = selective_worker_draft(root_session_id, "src/second.rs", &[]);
    isolated.workspace_strategy = WorkspaceStrategy::Isolated;
    let unavailable = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), isolated.clone(), false)
        .await
        .expect_err("isolated handoff requires a configured typed integrator");
    assert!(matches!(
        unavailable,
        StoreError::AdmissionRejected {
            reason: AdmissionRejectionReason::IsolatedIntegratorUnavailable,
            reusable_assignment_id: None,
        }
    ));
    let admitted = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), isolated, true)
        .await
        .expect("configured typed integrator makes the isolated handoff feasible");
    assert_eq!(
        admitted.integration_plan,
        IntegrationPlan::TypedIntegratorRequired
    );
    assert_eq!(
        admitted.assignment.integration_plan,
        IntegrationPlan::TypedIntegratorRequired
    );
    assert_eq!(
        fixture
            .store
            .get_agent_task(admitted.assignment.assignment_id, Some(0))
            .await
            .expect("persisted assignment reloads")
            .assignment
            .integration_plan,
        IntegrationPlan::TypedIntegratorRequired
    );
}

#[tokio::test]
async fn selective_admission_admits_writes_over_active_verification_proof_ownership() {
    let fixture = Fixture::new().await;
    let root_session_id = "active-verification-admission-root";
    let worker_command = "cargo test -p owner worker-proof";
    let (worker, worker_attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft(root_session_id, "src/verified.rs", worker_command),
        )
        .await
        .expect("worker assignment");
    finish_focused_validation(
        &fixture.store,
        start_focused_validation(
            &fixture.store,
            worker_attempt.attempt_id,
            "worker-proof-call",
            worker_command,
        )
        .await,
    )
    .await;
    fixture
        .store
        .submit_agent_receipt(
            worker_attempt.attempt_id,
            completed_receipt(vec!["worker-proof-call".to_string()]),
        )
        .await
        .expect("worker receipt");

    let verifier_command = "cargo test -p owner verifier-proof";
    let mut verifier = relation_draft(root_session_id, AgentRole::Verifier, worker.assignment_id);
    verifier.required_evidence = vec![verifier_command.to_string()];
    let verifier = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), verifier, true)
        .await
        .expect("verifier assignment");
    start_focused_validation(
        &fixture.store,
        verifier.attempt.attempt_id,
        "verifier-proof-call",
        verifier_command,
    )
    .await;

    let admitted = fixture
        .store
        .create_admitted_assignment(
            fixture.repo.path(),
            selective_worker_draft(root_session_id, "src/verified.rs", &[]),
            true,
        )
        .await
        .expect("an active verification proof is advisory to a new writer");
    assert_eq!(admitted.integration_plan, IntegrationPlan::SingleWriter);
}

#[tokio::test]
async fn dependency_validation_returns_every_blocker() {
    let fixture = Fixture::new().await;
    let (incomplete, _) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "first"))
        .await
        .expect("incomplete dependency assignment");
    let unknown = AssignmentId::new();
    let mut candidate = worker_draft("root", "second");
    candidate.dependencies = vec![incomplete.assignment_id, unknown];
    let error = fixture
        .store
        .create_assignment(fixture.repo.path(), candidate)
        .await
        .expect_err("both dependencies block");
    let StoreError::DependencyBlocked { blockers } = error else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(blockers.len(), 2);
    assert_eq!(
        blockers
            .iter()
            .map(|blocker| blocker.state)
            .collect::<Vec<_>>(),
        vec![DependencyState::Incomplete, DependencyState::Unknown]
    );
}

#[tokio::test]
async fn oversized_receipts_seal_and_remain_fully_retrievable() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("oversized-receipt-root", "src/lib.rs"),
        )
        .await
        .expect("oversized receipt assignment");
    let summary = "durable oversized receipt evidence ".repeat(2_000);
    let blockers = vec!["durable blocker evidence ".repeat(1_000)];
    let receipt = fixture
        .store
        .submit_agent_receipt(
            attempt.attempt_id,
            ReceiptDraft {
                status: AgentStatusClaim::NeedsMain,
                summary: summary.clone(),
                criterion_results: vec![CriterionResult {
                    evidence_ref: None,
                    criterion_id: criterion().id,
                    status: CriterionStatus::NotRun,
                    evidence: None,
                }],
                declared_changes: Vec::new(),
                validation_call_ids: Vec::new(),
                blockers: blockers.clone(),
                risks: vec!["durable risk evidence ".repeat(1_000)],
                next_action: Some("root must resolve the durable blocker".to_string()),
                architecture_contract: None,
            },
        )
        .await
        .expect("large prose does not reject receipt sealing");
    assert_eq!(receipt.summary, summary);

    let task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("sealed oversized receipt remains readable");
    let stored = task.receipt.expect("sealed receipt");
    assert_eq!(stored.summary, summary);
    assert_eq!(stored.blockers, blockers);
}

#[tokio::test]
async fn receipts_are_sealed_and_validation_calls_are_attempt_owned() {
    let fixture = Fixture::new().await;
    let mut first_draft = worker_draft("root", "first");
    first_draft.required_evidence = vec!["focused test".to_string()];
    let (first, first_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), first_draft)
        .await
        .expect("first assignment");
    let (_, second_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "second"))
        .await
        .expect("second assignment");
    let call = start_focused_validation(
        &fixture.store,
        first_attempt.attempt_id,
        "call-1",
        "focused test",
    )
    .await;
    finish_focused_validation(&fixture.store, call).await;
    assert!(
        matches!(
            fixture
                .store
                .submit_agent_receipt(
                    second_attempt.attempt_id,
                    completed_receipt(vec!["call-1".to_string()]),
                )
                .await,
            Err(StoreError::ValidationCallOwnership { .. })
        ),
        "cross-attempt validation call must be rejected"
    );
    fixture
        .store
        .submit_agent_receipt(
            first_attempt.attempt_id,
            completed_receipt(vec!["call-1".to_string()]),
        )
        .await
        .expect("owned validation call seals receipt");
    assert!(
        fixture
            .store
            .submit_agent_receipt(first_attempt.attempt_id, completed_receipt(Vec::new()))
            .await
            .is_err()
    );
    let task = fixture
        .store
        .get_agent_task(first.assignment_id, Some(100))
        .await
        .expect("task reloads");
    assert_eq!(
        task.receipt.expect("sealed receipt").status,
        AgentStatusClaim::Completed
    );
}

#[tokio::test]
async fn criterion_execution_reference_is_validated_persisted_and_projected() {
    let fixture = Fixture::new().await;
    initialize_validation_repository(fixture.repo.path());
    let command = "cargo test -p evidence focused-proof";
    let mut assignment_draft =
        validation_worker_draft("criterion-evidence-root", "src/lib.rs", command);
    assignment_draft
        .acceptance_criteria
        .push(AcceptanceCriterion {
            id: "unverified-outcome".to_string(),
            text: "a separately promised outcome".to_string(),
        });
    assignment_draft
        .acceptance_criteria
        .push(AcceptanceCriterion {
            id: "shared-proof".to_string(),
            text: "another criterion supported by the same execution".to_string(),
        });
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), assignment_draft)
        .await
        .expect("assignment");
    let call = finish_focused_validation(
        &fixture.store,
        start_focused_validation(
            &fixture.store,
            attempt.attempt_id,
            "criterion-proof",
            command,
        )
        .await,
    )
    .await;
    let reference = CriterionEvidenceRef {
        call_id: call.call_id.clone(),
        workspace_id: assignment.workspace_id.clone(),
        evidence_epoch: call.evidence.end_epoch.expect("recorded epoch"),
        kind: CriterionEvidenceKind::ValidationExecution,
    };
    let mut draft = completed_receipt(vec![call.call_id.clone()]);
    draft.summary = "Every outcome tested; the new Desktop build is running".to_string();
    draft.criterion_results[0].evidence_ref = Some(reference.clone());
    draft.criterion_results.push(CriterionResult {
        criterion_id: "unverified-outcome".to_string(),
        status: CriterionStatus::Passed,
        evidence: Some("trust the narrative".to_string()),
        evidence_ref: None,
    });
    draft.criterion_results.push(CriterionResult {
        criterion_id: "shared-proof".to_string(),
        status: CriterionStatus::Passed,
        evidence: None,
        evidence_ref: Some(reference.clone()),
    });
    for invalid in [
        CriterionEvidenceRef {
            workspace_id: "another-workspace".to_string(),
            ..reference.clone()
        },
        CriterionEvidenceRef {
            evidence_epoch: reference.evidence_epoch + 1,
            ..reference.clone()
        },
        CriterionEvidenceRef {
            call_id: "unlisted-or-foreign-call".to_string(),
            ..reference.clone()
        },
        CriterionEvidenceRef {
            kind: CriterionEvidenceKind::SourceInspection,
            ..reference.clone()
        },
    ] {
        let mut rejected = draft.clone();
        rejected.criterion_results[0].evidence_ref = Some(invalid);
        assert!(matches!(
            fixture
                .store
                .submit_agent_receipt(attempt.attempt_id, rejected)
                .await,
            Err(StoreError::CriterionResultsInvalid(_))
        ));
        let task = fixture
            .store
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .expect("unsealed task");
        assert!(task.receipt.is_none());
        assert_eq!(task.current_attempt.state, AttemptState::Active);
    }
    fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, draft)
        .await
        .expect("valid reference seals");
    let mut task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("sealed task reloads");
    assert_eq!(
        task.receipt.as_ref().unwrap().criterion_results[0]
            .evidence_ref
            .as_ref(),
        Some(&reference)
    );
    let summary = task.completion_evidence_summary();
    assert!(summary.contains("criterion-1: supported by a successful validation execution"));
    assert!(summary.contains("unverified-outcome: reported complete; behavior unverified"));
    assert!(summary.contains("shared-proof: supported by a successful validation execution"));
    assert_eq!(
        task.receipt.as_ref().unwrap().validation_call_ids,
        vec![call.call_id]
    );
    assert!(summary.contains("Running Desktop build: not established"));
    assert!(!summary.contains("Every outcome tested"));
    // The loaded workspace can advance without upgrading or rerunning old proof.
    task.workspace_status.epoch += 1;
    assert!(
        task.completion_evidence_summary()
            .contains("freshness unverified")
    );
    task.validation_calls[0].status = ValidationCallStatus::Cancelled;
    assert!(
        !task
            .completion_evidence_summary()
            .contains("supported by a successful")
    );
    // Old persisted JSON remains readable and acquires no proof from prose.
    let mut legacy = serde_json::to_value(task.receipt.as_ref().unwrap()).unwrap();
    legacy["criterion_results"][0]
        .as_object_mut()
        .unwrap()
        .remove("evidence_ref");
    task.receipt = Some(serde_json::from_value(legacy).unwrap());
    assert!(
        task.receipt.as_ref().unwrap().criterion_results[0]
            .evidence_ref
            .is_none()
    );
}

#[tokio::test]
async fn source_inspection_receipts_bind_kind_without_workspace_tracking() {
    use sha2::Digest;
    let fixture = Fixture::new().await;
    initialize_validation_repository(fixture.repo.path());
    let path = fixture.repo.path().join("src/lib.rs");
    let (assignment, attempt) = fixture.store.create_assignment(
        fixture.repo.path(),
        validation_worker_draft("inspection", "src/lib.rs", "inspect:src/lib.rs"),
    ).await.unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let hash = format!("{:x}", sha2::Sha256::digest(&bytes));
    let fresh = fixture.store.prepare_source_inspection(
        attempt.attempt_id, path.to_string_lossy().into_owned(),
    ).await.unwrap().unwrap();
    let reference = fixture.store.record_source_inspection(
        fresh, "source-read".into(), hash, bytes.len() as u64,
    ).await.unwrap();
    assert_eq!(reference.kind, CriterionEvidenceKind::SourceInspection);
    let mut draft = completed_receipt(vec![reference.call_id.clone()]);
    draft.criterion_results[0].evidence_ref = Some(CriterionEvidenceRef {
        kind: CriterionEvidenceKind::ValidationExecution, ..reference.clone()
    });
    assert!(matches!(
        fixture.store.submit_agent_receipt(attempt.attempt_id, draft.clone()).await,
        Err(StoreError::CriterionResultsInvalid(_))
    ));
    draft.criterion_results[0].evidence_ref = Some(reference);
    fixture.store.submit_agent_receipt(attempt.attempt_id, draft).await.unwrap();
    let task = fixture.store.get_agent_task(assignment.assignment_id, Some(0)).await.unwrap();
    assert!(task.completion_evidence_summary().contains(
        "supported by complete source acquisition (semantic inspection and correctness not established; freshness unverified)"
    ));
    let stale_fixture = Fixture::new().await;
    initialize_validation_repository(stale_fixture.repo.path());
    let stale_path = stale_fixture.repo.path().join("src/lib.rs");
    let (_, stale_attempt) = stale_fixture.store.create_assignment(
        stale_fixture.repo.path(),
        validation_worker_draft("stale-inspection", "src/lib.rs", "inspect:src/lib.rs"),
    ).await.unwrap();
    let stale = stale_fixture.store.prepare_source_inspection(
        stale_attempt.attempt_id, stale_path.to_string_lossy().into_owned(),
    ).await.unwrap().unwrap();
    std::fs::write(&stale_path, "pub fn changed() {}\n").unwrap();
    let bytes = std::fs::read(&stale_path).unwrap();
    let hash = format!("{:x}", sha2::Sha256::digest(&bytes));
    let reference = stale_fixture.store.record_source_inspection(
        stale, "stale-read".into(), hash, bytes.len() as u64,
    ).await.expect("source inspection no longer scans workspace changes");
    assert_eq!(reference.kind, CriterionEvidenceKind::SourceInspection);
}

#[tokio::test]
async fn completed_receipt_retains_validation_without_workspace_tracking() {
    let fixture = Fixture::new().await;
    initialize_validation_repository(fixture.repo.path());
    let command = "cargo test -p freshness focused-proof";
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft("fresh-receipt-root", "src/lib.rs", command),
        )
        .await
        .expect("validation assignment creates");
    finish_focused_validation(
        &fixture.store,
        start_focused_validation(
            &fixture.store,
            attempt.attempt_id,
            "fresh-receipt-call",
            command,
        )
        .await,
    )
    .await;

    std::fs::write(
        fixture.repo.path().join("src/lib.rs"),
        "pub fn changed_after_validation() {}\n",
    )
    .expect("source changes after validation");
    let mut draft = completed_receipt(vec!["fresh-receipt-call".to_string()]);
    draft.criterion_results[0].evidence_ref = Some(CriterionEvidenceRef {
        kind: CriterionEvidenceKind::ValidationExecution,
        call_id: "fresh-receipt-call".to_string(),
        workspace_id: assignment.workspace_id.clone(),
        evidence_epoch: 0,
    });
    fixture.store.submit_agent_receipt(attempt.attempt_id, draft).await
        .expect("workspace changes no longer trigger automatic freshness rejection");
    let task = fixture.store.get_agent_task(assignment.assignment_id, Some(0)).await.unwrap();
    assert!(task.receipt.is_some());
    assert!(task.completion_evidence_summary().contains("freshness unverified"));
    assert!(!fixture.state.codex_home().join("agent-task-coordination").join("snapshots").exists());
    let pool = coordination_pool(&fixture).await;
    let count: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM write_claims)
              + (SELECT COUNT(*) FROM contract_claims)
              + (SELECT COUNT(*) FROM workspace_paths)
              + (SELECT COUNT(*) FROM workspace_events)
              + (SELECT COUNT(*) FROM mutation_files)
              + (SELECT COUNT(*) FROM isolated_handoffs)",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 0, "workspace tracking tables must not receive new records");
    pool.close().await;
    fixture.store.close().await;
    let restarted = LocalAgentTaskStore::initialize(&fixture.state).await.unwrap();
    let reloaded = restarted.get_agent_task(assignment.assignment_id, Some(0)).await.unwrap();
    assert_eq!(reloaded.receipt, task.receipt);
    restarted.close().await;
}

#[tokio::test]
async fn missing_evidence_is_rebuilt_from_current_calls_on_every_submission() {
    let fixture = Fixture::new().await;
    let first_command = "cargo test -p first";
    let second_command = "cargo test -p second";
    let mut draft = worker_draft("current-evidence-root", "src");
    draft.required_evidence = vec![first_command.to_string(), second_command.to_string()];
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("worker assignment");
    bind_test_agent(
        &fixture.store,
        assignment.assignment_id,
        attempt.attempt_id,
        "current-evidence-root",
    )
    .await;
    let empty_receipt = completed_receipt(Vec::new());
    let initial_error = fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, empty_receipt)
        .await
        .expect_err("both current results are initially missing");
    let StoreError::RequiredEvidenceMissing {
        obligations: initial_obligations,
    } = initial_error
    else {
        panic!("unexpected error: {initial_error}");
    };
    assert_eq!(initial_obligations.len(), 2);
    assert!(
        initial_obligations[0]
            .id
            .contains(&assignment.assignment_id.to_string())
    );
    assert!(initial_obligations[0].id.contains(":0001:"));
    assert!(initial_obligations[1].id.contains(":0002:"));

    let first = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "current-first",
        first_command,
    )
    .await;
    finish_focused_validation(&fixture.store, first).await;
    let partial_receipt = completed_receipt(vec!["current-first".to_string()]);
    let error = fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, partial_receipt)
        .await
        .expect_err("the current gate still requires the second result");
    let StoreError::RequiredEvidenceMissing { obligations } = error else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(obligations.len(), 1);
    assert_eq!(obligations[0].requirement, second_command);

    let second = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "current-second",
        second_command,
    )
    .await;
    finish_focused_validation(&fixture.store, second).await;
    let receipt = fixture
        .store
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt(vec![
                "current-first".to_string(),
                "current-second".to_string(),
            ]),
        )
        .await
        .expect("the gate rebuild sees both current successful results");
    assert_eq!(receipt.status, AgentStatusClaim::Completed);
}

#[tokio::test]
async fn host_legacy_outcome_persists_final_response_without_fabricating_evidence() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("plain-message-root", ".");
    draft.admission_origin = AssignmentAdmissionOrigin::LegacyMessage {
        parent_assignment_id: None,
    };
    draft.workspace_strategy = WorkspaceStrategy::Shared;
    // Older persisted plain-message tasks used this synthetic, non-command obligation.
    draft.required_evidence = vec!["task result reported to the parent agent".to_string()];
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("plain-message assignment creates");
    controlled_write(
        &fixture.store,
        fixture.repo.path(),
        "plain-message-root",
        assignment.assignment_id,
        attempt.attempt_id,
        "result.txt",
        "changed",
    )
    .await;
    fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            assignment.assignment_id,
            GateKind::Verification,
            GateStatus::Pending,
            "advisory verification not run".to_string(),
        )
        .await
        .unwrap();
    let receipt = fixture
        .store
        .record_legacy_agent_outcome(
            attempt.attempt_id,
            AgentStatusClaim::Completed,
            "Implemented the requested change; tests were not run.".to_string(),
        )
        .await
        .expect("host-observed final response seals ordinary task");
    assert_eq!(receipt.status, AgentStatusClaim::Completed);
    assert_eq!(receipt.criterion_results.len(), 1);
    assert_eq!(receipt.criterion_results[0].status, CriterionStatus::NotRun);
    assert_eq!(receipt.criterion_results[0].evidence, None);
    assert_eq!(receipt.criterion_results[0].evidence_ref, None);
    assert!(receipt.validation_call_ids.is_empty());
    assert!(receipt.declared_changes.is_empty());
    let task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("host outcome reloads");
    assert_eq!(task.current_attempt.state, AttemptState::Completed);
    assert_eq!(
        task.workspace_status.pending_gates,
        vec![GateKind::Verification]
    );
    assert_eq!(task.workspace_status.next_required_action, None);
    assert_eq!(task.receipt, Some(receipt));
    assert!(matches!(
        fixture
            .store
            .record_legacy_agent_outcome(
                attempt.attempt_id,
                AgentStatusClaim::Completed,
                "second final response".to_string(),
            )
            .await,
        Err(StoreError::AttemptSealed(_))
    ));
}

#[tokio::test]
async fn host_legacy_outcome_cannot_bypass_explicit_typed_receipt() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("typed-root", "src"))
        .await
        .expect("typed assignment creates");
    assert!(matches!(
        fixture
            .store
            .record_legacy_agent_outcome(
                attempt.attempt_id,
                AgentStatusClaim::Completed,
                "done".to_string(),
            )
            .await,
        Err(StoreError::InvalidAssignment(_))
    ));
    let task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("rejected completion leaves task readable");
    assert_eq!(task.current_attempt.state, AttemptState::Active);
    assert!(task.receipt.is_none());
    let mut unsupported = completed_receipt(Vec::new());
    unsupported.criterion_results[0].status = CriterionStatus::NotRun;
    assert!(matches!(
        fixture
            .store
            .submit_agent_receipt(attempt.attempt_id, unsupported)
            .await,
        Err(StoreError::CriterionResultsInvalid(_))
    ));
}


#[tokio::test]
async fn host_legacy_outcome_waits_for_running_validation() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("plain-validation-root", ".");
    draft.admission_origin = AssignmentAdmissionOrigin::LegacyMessage {
        parent_assignment_id: None,
    };
    draft.workspace_strategy = WorkspaceStrategy::Shared;
    draft.required_evidence = vec!["cargo test -p plain".to_string()];
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("plain-message assignment creates");
    let running = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "plain-running-check",
        "cargo test -p plain",
    )
    .await;
    assert!(matches!(
        fixture.store.record_legacy_agent_outcome(
            attempt.attempt_id, AgentStatusClaim::Completed, "done".to_string(),
        ).await,
        Err(StoreError::ValidationCallStatusInvalid { call_ids })
            if call_ids == vec![running.call_id.clone()]
    ));
    assert!(
        fixture
            .store
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .unwrap()
            .receipt
            .is_none()
    );
    finish_focused_validation(&fixture.store, running).await;
    let receipt = fixture
        .store
        .record_legacy_agent_outcome(
            attempt.attempt_id,
            AgentStatusClaim::Completed,
            "done".to_string(),
        )
        .await
        .expect("settled validation allows host outcome");
    assert!(
        receipt.validation_call_ids.is_empty(),
        "host must not infer which behavior a completed validation proved"
    );
}

#[tokio::test]
async fn host_legacy_followup_renews_each_completed_turn_and_its_binding() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("plain-followup-root", ".");
    draft.admission_origin = AssignmentAdmissionOrigin::LegacyMessage {
        parent_assignment_id: None,
    };
    draft.workspace_strategy = WorkspaceStrategy::Shared;
    draft.required_evidence.clear();
    let (assignment, mut attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .unwrap();
    let original_binding = bind_test_agent(
        &fixture.store,
        assignment.assignment_id,
        attempt.attempt_id,
        "plain-followup-root",
    )
    .await;
    let mut prior_attempts = Vec::new();
    for ordinal in 0..3 {
        assert_eq!(attempt.ordinal, ordinal);
        // A follow-up queued while this turn is active must not retire its tools or receipt.
        assert_eq!(
            fixture
                .store
                .begin_legacy_agent_turn(assignment.assignment_id)
                .await
                .unwrap(),
            attempt
        );
        let summary = format!("completed turn {ordinal}");
        fixture
            .store
            .record_legacy_agent_outcome(
                attempt.attempt_id,
                AgentStatusClaim::Completed,
                summary.clone(),
            )
            .await
            .unwrap();
        let completed = fixture
            .store
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .unwrap();
        assert_eq!(completed.receipt.unwrap().summary, summary);
        prior_attempts.push(attempt.attempt_id);
        attempt = fixture
            .store
            .begin_legacy_agent_turn(assignment.assignment_id)
            .await
            .unwrap();
        assert_eq!(attempt.ordinal, ordinal + 1);
        let binding = fixture
            .store
            .get_agent_task_binding(assignment.assignment_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.attempt_id, attempt.attempt_id);
        assert_eq!(binding.thread_id, original_binding.thread_id);
        assert_eq!(binding.agent_path, original_binding.agent_path);
        let active = fixture
            .store
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .unwrap();
        assert_eq!(active.current_attempt.state, AttemptState::Active);
        assert!(active.receipt.is_none());
        assert!(active.validation_calls.is_empty());
    }
    let receipt = fixture
        .store
        .record_legacy_agent_outcome(
            attempt.attempt_id,
            AgentStatusClaim::Completed,
            "latest result".to_string(),
        )
        .await
        .unwrap();
    assert!(receipt.declared_changes.is_empty());
    assert!(receipt.validation_call_ids.is_empty());
    for old_attempt in prior_attempts {
        assert!(matches!(
            fixture
                .store
                .record_legacy_agent_outcome(
                    old_attempt,
                    AgentStatusClaim::Completed,
                    "stale result".to_string()
                )
                .await,
            Err(StoreError::AttemptSealed(_))
        ));
    }
    let latest = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .unwrap();
    assert_eq!(latest.receipt.unwrap().summary, "latest result");
}

#[tokio::test]
async fn host_legacy_followup_cannot_renew_typed_assignments() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("typed-followup-root", "src"),
        )
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .store
            .begin_legacy_agent_turn(assignment.assignment_id)
            .await,
        Err(StoreError::InvalidAssignment(_))
    ));
    let unchanged = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .unwrap();
    assert_eq!(unchanged.current_attempt.attempt_id, attempt.attempt_id);
    assert_eq!(unchanged.current_attempt.state, AttemptState::Active);
}

#[tokio::test]
async fn receipt_sealing_waits_for_all_attempt_owned_running_validations() {
    let fixture = Fixture::new().await;
    let command = "cargo test -p receipt-seal";
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft("receipt-seal-root", "src", command),
        )
        .await
        .expect("validation assignment creates");
    let running = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "receipt-seal-running",
        command,
    )
    .await;
    let receipt = ReceiptDraft {
        status: AgentStatusClaim::NeedsMain,
        summary: "main agent must reconcile the outcome".to_string(),
        criterion_results: vec![CriterionResult {
            evidence_ref: None,
            criterion_id: criterion().id,
            status: CriterionStatus::NotRun,
            evidence: None,
        }],
        declared_changes: Vec::new(),
        validation_call_ids: Vec::new(),
        blockers: vec!["validation watcher is still running".to_string()],
        risks: Vec::new(),
        next_action: Some("wait for the watcher".to_string()),
        architecture_contract: None,
    };

    assert!(matches!(
        fixture
            .store
            .submit_agent_receipt(attempt.attempt_id, receipt.clone())
            .await,
        Err(StoreError::ValidationCallStatusInvalid { call_ids })
            if call_ids == vec![running.call_id.clone()]
    ));
    assert!(
        fixture
            .store
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .expect("unsealed task reads")
            .receipt
            .is_none()
    );

    finish_focused_validation(&fixture.store, running).await;
    fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, receipt)
        .await
        .expect("receipt seals after the watcher finishes");
    let task = fixture.store.get_agent_task(assignment.assignment_id, Some(0)).await.unwrap();
    assert!(task.receipt.is_some());
    assert!(task.validation_calls.iter().all(|call| call.status != ValidationCallStatus::Running));
}

#[tokio::test]
async fn validation_calls_allow_only_running_to_terminal_transitions() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("root", "src");
    draft.required_evidence = vec!["focused test".to_string()];
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("worker assignment");
    let started_at = Utc::now();
    fixture
        .store
        .record_validation_call(ValidationCall {
            call_id: "direct-argv-only".to_string(),
            attempt_id: attempt.attempt_id,
            command_summary: "focused test".to_string(),
            evidence: ValidationEvidence::default(),
            status: ValidationCallStatus::Running,
            recorded_at: started_at,
        })
        .await
        .expect("validation calls do not require executable provenance");
    assert!(matches!(
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: "missing-start".to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "focused test".to_string(),
                evidence: ValidationEvidence::default(),
                status: ValidationCallStatus::Succeeded,
                recorded_at: started_at,
            })
            .await,
        Err(StoreError::ValidationCallImmutable(_))
    ));
    assert!(matches!(
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: "wrong-command".to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "cargo test -p other".to_string(),
                evidence: ValidationEvidence::default(),
                status: ValidationCallStatus::Running,
                recorded_at: started_at,
            })
            .await,
        Err(StoreError::InvalidAssignment(_))
    ));
    fixture
        .store
        .record_validation_call(ValidationCall {
            call_id: "transition".to_string(),
            attempt_id: attempt.attempt_id,
            command_summary: "focused test".to_string(),
            evidence: ValidationEvidence::default(),
            status: ValidationCallStatus::Running,
            recorded_at: started_at,
        })
        .await
        .expect("running call records");
    assert!(matches!(
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: "transition".to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "changed command".to_string(),
                evidence: ValidationEvidence::default(),
                status: ValidationCallStatus::Succeeded,
                recorded_at: started_at + Duration::milliseconds(500),
            })
            .await,
        Err(StoreError::ValidationCallImmutable(_))
    ));
    fixture
        .store
        .record_validation_call(ValidationCall {
            call_id: "transition".to_string(),
            attempt_id: attempt.attempt_id,
            command_summary: "focused test".to_string(),
            evidence: ValidationEvidence {
                validation_result: Some(serde_json::json!({
                    "argv": ["focused test"],
                    "coveredPaths": ["."],
                    "callId": "transition",
                    "processId": null,
                    "status": "succeeded",
                    "durationMs": 1,
                })),
                ..ValidationEvidence::default()
            },
            status: ValidationCallStatus::Succeeded,
            recorded_at: started_at + Duration::seconds(1),
        })
        .await
        .expect("running call becomes terminal");
    assert!(matches!(
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: "transition".to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "focused test".to_string(),
                evidence: ValidationEvidence::default(),
                status: ValidationCallStatus::Failed,
                recorded_at: started_at + Duration::seconds(2),
            })
            .await,
        Err(StoreError::ValidationCallImmutable(_))
    ));

    for call_id in ["still-running", "failed", "cancelled"] {
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: call_id.to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "focused test".to_string(),
                evidence: ValidationEvidence::default(),
                status: ValidationCallStatus::Running,
                recorded_at: started_at + Duration::seconds(3),
            })
            .await
            .expect("additional validation call starts");
    }
    for (call_id, status) in [
        ("failed", ValidationCallStatus::Failed),
        ("cancelled", ValidationCallStatus::Cancelled),
    ] {
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: call_id.to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "focused test".to_string(),
                evidence: ValidationEvidence::default(),
                status,
                recorded_at: started_at + Duration::seconds(4),
            })
            .await
            .expect("additional validation call finishes");
    }
    let error = fixture
        .store
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt(vec![
                "still-running".to_string(),
                "failed".to_string(),
                "cancelled".to_string(),
            ]),
        )
        .await
        .expect_err("completed receipt rejects non-successful calls");
    let StoreError::ValidationCallStatusInvalid { call_ids } = error else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(
        call_ids,
        vec![
            "still-running".to_string(),
            "failed".to_string(),
            "cancelled".to_string()
        ]
    );
    let task = fixture
        .store
        .get_agent_task(attempt.assignment_id, Some(0))
        .await
        .expect("validation calls reload");
    assert_eq!(task.validation_calls.len(), 5);
    assert_eq!(
        task.validation_calls
            .iter()
            .map(|call| call.call_id.as_str())
            .collect::<HashSet<_>>()
            .len(),
        5
    );
    for mut call in task.validation_calls {
        if call.status == ValidationCallStatus::Running {
            call.status = ValidationCallStatus::Cancelled;
            call.recorded_at = started_at + Duration::seconds(5);
            fixture
                .store
                .record_validation_call(call)
                .await
                .expect("remaining calls cancel");
        }
    }
    fixture
        .store
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt(vec!["transition".to_string()]),
        )
        .await
        .expect("successful terminal call seals receipt");
    assert!(matches!(
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: "after-seal".to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "too late".to_string(),
                evidence: ValidationEvidence::default(),
                status: ValidationCallStatus::Succeeded,
                recorded_at: Utc::now(),
            })
            .await,
        Err(StoreError::AttemptNotActive(_))
    ));
}

#[tokio::test]
async fn focused_validation_start_rejects_unauthorized_roles() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("root", "src");
    draft.role = AgentRole::Explorer;
    draft.capability_profile = CapabilityProfile::ReadSearch;
    draft.write_scope.clear();
    draft.required_evidence = vec!["focused test".to_string()];
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("explorer assignment");
    assert!(matches!(
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: "explorer-validation".to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "focused test".to_string(),
                evidence: ValidationEvidence::default(),
                status: ValidationCallStatus::Running,
                recorded_at: Utc::now(),
            })
            .await,
        Err(StoreError::InvalidAssignment(_))
    ));
}

#[tokio::test]
async fn validation_call_rejects_removed_proof_fields() {
    let current = ValidationCall {
        call_id: "legacy".to_string(),
        attempt_id: AttemptId::new(),
        command_summary: "focused test".to_string(),
        evidence: ValidationEvidence::default(),
        status: ValidationCallStatus::Running,
        recorded_at: Utc::now(),
    };
    let mut legacy_json = serde_json::to_value(current).expect("validation call serializes");
    let object = legacy_json
        .as_object_mut()
        .expect("validation call is an object");
    object.insert("proof_kind".to_string(), serde_json::json!("focused"));
    object.insert(
        "resolved_executable".to_string(),
        serde_json::json!("/tmp/test-runner"),
    );
    serde_json::from_value::<ValidationCall>(legacy_json)
        .expect_err("removed proof fields are rejected");
}

#[tokio::test]
async fn malformed_validation_result_cannot_satisfy_completion() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("strict-result-root", "src");
    draft.required_evidence = vec!["focused test".to_string()];
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("worker assignment");
    let mut call = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "malformed-result",
        "focused test",
    )
    .await;
    call.evidence.validation_result = Some(serde_json::json!({
        "argv": ["focused test"],
        "coveredPaths": ["src"],
        "callId": "malformed-result",
        "status": "succeeded",
        "durationMs": 1,
        "proofKey": "removed",
    }));
    call.status = ValidationCallStatus::Succeeded;
    call.recorded_at += Duration::milliseconds(1);
    fixture
        .store
        .record_validation_call(call)
        .await
        .expect("terminal result records for audit");

    assert!(matches!(
        fixture
            .store
            .submit_agent_receipt(
                attempt.attempt_id,
                completed_receipt(vec!["malformed-result".to_string()]),
            )
            .await,
        Err(StoreError::ValidationCallStatusInvalid { .. })
    ));
}

#[tokio::test]
async fn non_normalized_validation_result_paths_cannot_satisfy_completion() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("strict-path-root", "src");
    draft.required_evidence = vec!["focused test".to_string()];
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("worker assignment");

    for (index, covered_paths) in [
        serde_json::json!(["../outside"]),
        serde_json::json!(["src/./lib.rs"]),
        serde_json::json!(["src", "src"]),
        serde_json::json!(["src", "SRC"]),
        serde_json::json!(["/absolute"]),
    ]
    .into_iter()
    .enumerate()
    {
        let call_id = format!("malformed-path-{index}");
        let mut call =
            start_focused_validation(&fixture.store, attempt.attempt_id, &call_id, "focused test")
                .await;
        call.evidence.validation_result = Some(serde_json::json!({
            "argv": ["focused test"],
            "coveredPaths": covered_paths,
            "callId": call_id,
            "status": "succeeded",
            "durationMs": 1,
        }));
        call.status = ValidationCallStatus::Succeeded;
        call.recorded_at += Duration::milliseconds(1);
        fixture
            .store
            .record_validation_call(call)
            .await
            .expect("malformed terminal result remains audit material");

        assert!(matches!(
            fixture
                .store
                .submit_agent_receipt(attempt.attempt_id, completed_receipt(vec![call_id]),)
                .await,
            Err(StoreError::ValidationCallStatusInvalid { .. })
        ));
    }
}

#[tokio::test]
async fn agent_task_bindings_persist_and_are_root_session_scoped() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("binding-root", "src"))
        .await
        .expect("worker assignment");
    let expected = fixture
        .store
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: assignment.assignment_id,
            attempt_id: attempt.attempt_id,
            agent_path: "/root/worker".to_string(),
            task_name: "worker".to_string(),
            thread_id: Some("thread-1".to_string()),
        })
        .await
        .expect("binding persists");
    let (foreign, foreign_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("other-binding-root", "src"))
        .await
        .expect("foreign root assignment");
    fixture.store.bind_agent_task(AgentTaskBindingDraft {
        assignment_id: foreign.assignment_id,
        attempt_id: foreign_attempt.attempt_id,
        agent_path: expected.agent_path.clone(),
        task_name: expected.task_name.clone(),
        thread_id: expected.thread_id.clone(),
    }).await.expect("binding names may repeat in another root");
    assert_eq!(
        fixture
            .store
            .get_agent_task_binding(assignment.assignment_id)
            .await
            .expect("binding lookup"),
        Some(expected.clone())
    );
    assert_eq!(
        fixture
            .store
            .list_agent_task_bindings("binding-root".to_string(), None)
            .await
            .expect("binding list"),
        vec![expected.clone()]
    );

    fixture.store.close().await;
    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("store restarts");
    assert_eq!(
        restarted
            .get_agent_task_binding(assignment.assignment_id)
            .await
            .expect("binding survives restart"),
        Some(expected)
    );
}

#[tokio::test]
async fn sealed_failed_start_binding_can_be_removed_without_deleting_task_history() {
    let fixture = Fixture::new().await;
    let root_session_id = "failed-start-root";
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft(root_session_id, "src"))
        .await
        .expect("worker assignment");
    fixture
        .store
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: assignment.assignment_id,
            attempt_id: attempt.attempt_id,
            agent_path: "/root/retryable_worker".to_string(),
            task_name: "retryable_worker".to_string(),
            thread_id: Some("failed-thread".to_string()),
        })
        .await
        .expect("failed-start task binds before initial submission");

    assert!(matches!(
        fixture
            .store
            .remove_agent_task_binding(TaskActor::Root, assignment.assignment_id)
            .await,
        Err(StoreError::InvalidAssignment(_))
    ));

    fixture
        .store
        .abandon_agent_task(
            TaskActor::Root,
            assignment.assignment_id,
            "initial submission failed".to_string(),
        )
        .await
        .expect("failed-start assignment is durably abandoned");
    assert!(
        fixture
            .store
            .remove_agent_task_binding(TaskActor::Root, assignment.assignment_id)
            .await
            .expect("sealed failed-start binding can be removed")
    );
    assert_eq!(
        fixture
            .store
            .get_agent_task_binding(assignment.assignment_id)
            .await
            .expect("removed binding lookup"),
        None
    );
    let abandoned = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("abandoned task history remains readable");
    assert_eq!(abandoned.current_attempt.state, AttemptState::Abandoned);
    assert_eq!(
        abandoned
            .receipt
            .expect("abandonment receipt remains durable")
            .status,
        AgentStatusClaim::Abandoned
    );

    let (retry_assignment, retry_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft(root_session_id, "src"))
        .await
        .expect("retry assignment is admitted after abandonment");
    let retry_binding = fixture
        .store
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: retry_assignment.assignment_id,
            attempt_id: retry_attempt.attempt_id,
            agent_path: "/root/retryable_worker".to_string(),
            task_name: "retryable_worker".to_string(),
            thread_id: Some("retry-thread".to_string()),
        })
        .await
        .expect("removed failed-start binding allows the canonical path to be retried");
    assert_eq!(retry_binding.assignment_id, retry_assignment.assignment_id);
}

#[tokio::test]
async fn correction_attempt_is_immutable_and_bounded_to_one() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "src"))
        .await
        .expect("worker assignment");
    fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            assignment.assignment_id,
            GateKind::Review,
            GateStatus::Pending,
            "cold review required".to_string(),
        )
        .await
        .expect("pending gate");
    fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, completed_receipt(Vec::new()))
        .await
        .expect("worker receipt");
    fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            assignment.assignment_id,
            GateKind::Review,
            GateStatus::ChangesRequested,
            "one correction is required".to_string(),
        )
        .await
        .expect("changes requested");
    let amendment = AttemptAmendment {
        reason: "address cold review finding".to_string(),
        objective: None,
        acceptance_criteria: None,
        stop_condition: None,
    };
    let correction = fixture
        .store
        .amend_agent_task(TaskActor::Root, assignment.assignment_id, amendment.clone())
        .await
        .expect("single correction attempt");
    assert_eq!(correction.ordinal, 1);
    assert_eq!(correction.amendment, Some(amendment.clone()));
    assert!(matches!(
        fixture
            .store
            .append_observation(
                attempt.attempt_id,
                ObservationKind::Reading,
                "correction progress".to_string(),
                None,
            )
            .await,
        Err(StoreError::AttemptNotActive(_))
    ));
    fixture
        .store
        .append_observation(
            correction.attempt_id,
            ObservationKind::Reading,
            "correction progress".to_string(),
            None,
        )
        .await
        .expect("correction progress records");
    assert!(matches!(
        fixture
            .store
            .amend_agent_task(TaskActor::Root, assignment.assignment_id, amendment)
            .await,
        Err(StoreError::AmendmentLimitReached(_))
    ));
}

#[tokio::test]
async fn risk_review_progresses_to_independent_verification() {
    let fixture = Fixture::new().await;
    let (worker, worker_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("risk-root", "src"))
        .await
        .expect("worker assignment");
    fixture
        .store
        .submit_agent_receipt_with_review(
            worker_attempt.attempt_id,
            completed_receipt(Vec::new()),
            "cross-owner scope".to_string(),
        )
        .await
        .expect("risk-gated receipt");

    let task = fixture
        .store
        .get_agent_task(worker.assignment_id, Some(0))
        .await
        .expect("risk-gated task");
    assert!(
        task.gates
            .iter()
            .any(|gate| { gate.kind == GateKind::Risk && gate.status == GateStatus::Passed })
    );
    assert!(
        task.gates
            .iter()
            .any(|gate| { gate.kind == GateKind::Review && gate.status == GateStatus::Pending })
    );
    let (_, reviewer_attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            relation_draft("risk-root", AgentRole::Reviewer, worker.assignment_id),
        )
        .await
        .expect("matching reviewer may cross the pending review gate");
    fixture
        .store
        .set_agent_gate(
            TaskActor::Attempt(reviewer_attempt.attempt_id),
            worker.assignment_id,
            GateKind::Review,
            GateStatus::Passed,
            "cold review passed".to_string(),
        )
        .await
        .expect("review verdict");
    let reviewed = fixture
        .store
        .get_agent_task(worker.assignment_id, Some(0))
        .await
        .expect("reviewed task");
    assert!(
        reviewed.gates.iter().any(|gate| {
            gate.kind == GateKind::Verification && gate.status == GateStatus::Pending
        })
    );

    let (_, verifier_attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            relation_draft("risk-root", AgentRole::Verifier, worker.assignment_id),
        )
        .await
        .expect("matching verifier may cross the pending verification gate");
    fixture
        .store
        .set_agent_gate(
            TaskActor::Attempt(verifier_attempt.attempt_id),
            worker.assignment_id,
            GateKind::Verification,
            GateStatus::Passed,
            "independent verification passed".to_string(),
        )
        .await
        .expect("verification verdict");
    let task = fixture.store.get_agent_task(worker.assignment_id, Some(0)).await.unwrap();
    assert!(task.gates.iter().all(|gate| gate.status == GateStatus::Passed));
    assert_eq!(task.workspace_status.lease_state, Some(LeaseState::Released));
}

#[tokio::test]
async fn exact_typed_actor_heartbeat_renews_only_the_current_bound_attempt() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "before\n").expect("source");
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("heartbeat-root", "src/lib.rs"),
        )
        .await
        .expect("worker assignment");
    let binding = bind_test_agent(
        &fixture.store,
        assignment.assignment_id,
        attempt.attempt_id,
        "heartbeat-root",
    )
    .await;
    let comparison_now = expire_workspace_actor_leases(&fixture, &[attempt.attempt_id]).await;

    assert!(
        crate::local::with_test_comparison_now(
            comparison_now,
            fixture
                .store
                .heartbeat_typed_workspace_actor(binding.clone(), /*progress*/ false),
        )
        .await
        .expect("typed heartbeat")
    );
    let mut mismatched = binding.clone();
    mismatched.thread_id = Some("wrong-thread".to_string());
    assert!(
        !crate::local::with_test_comparison_now(
            comparison_now,
            fixture
                .store
                .heartbeat_typed_workspace_actor(mismatched, /*progress*/ false),
        )
        .await
        .expect("mismatched heartbeat is rejected")
    );

    fixture
        .store
        .abandon_agent_task(
            TaskActor::Root,
            assignment.assignment_id,
            "test terminal heartbeat".to_string(),
        )
        .await
        .expect("attempt is sealed");
    assert!(
        !crate::local::with_test_comparison_now(
            comparison_now,
            fixture
                .store
                .heartbeat_typed_workspace_actor(binding, /*progress*/ false),
        )
        .await
        .expect("sealed heartbeat is rejected")
    );
}

#[tokio::test]
async fn progress_heartbeat_defers_nonproductive_recovery_but_liveness_does_not() {
    let fixture = Fixture::new().await;
    let root_session_id = "progress-heartbeat-root";
    let pool = coordination_pool(&fixture).await;
    let stale = serde_json::to_string(&(Utc::now() - Duration::minutes(10)))
        .expect("stale timestamp serializes");
    let mut bindings = Vec::new();
    for scope in ["src/productive", "src/idle"] {
        let (assignment, attempt) = fixture
            .store
            .create_assignment(fixture.repo.path(), worker_draft(root_session_id, scope))
            .await
            .expect("worker assignment");
        bindings.push(
            bind_test_agent(
                &fixture.store,
                assignment.assignment_id,
                attempt.attempt_id,
                root_session_id,
            )
            .await,
        );
        // Both actors last progressed, and spent their one nudge, long ago.
        sqlx::query(
            "UPDATE workspace_actors SET last_progress_at = ?, nudge_sent_at = ?
             WHERE attempt_id = ?",
        )
        .bind(&stale)
        .bind(&stale)
        .bind(attempt.attempt_id.to_string())
        .execute(&pool)
        .await
        .expect("stale progress persists");
    }
    pool.close().await;
    let (productive, idle) = (&bindings[0], &bindings[1]);

    assert!(
        fixture
            .store
            .heartbeat_typed_workspace_actor(productive.clone(), /*progress*/ true)
            .await
            .expect("progress heartbeat")
    );
    assert!(
        fixture
            .store
            .heartbeat_typed_workspace_actor(idle.clone(), /*progress*/ false)
            .await
            .expect("liveness heartbeat")
    );
    let productive_task = fixture
        .store
        .get_agent_task(productive.assignment_id, Some(0))
        .await
        .expect("productive task reads");
    assert_eq!(productive_task.workspace_status.nudge_sent_at, None);
    let idle_task = fixture
        .store
        .get_agent_task(idle.assignment_id, Some(0))
        .await
        .expect("idle task reads");
    assert!(idle_task.workspace_status.nudge_sent_at.is_some());

    let no_progress_before = Utc::now() - Duration::seconds(DEFAULT_WORKSPACE_LEASE_SECONDS);
    assert_eq!(
        fixture
            .store
            .recover_nonproductive_assignment(productive.assignment_id, no_progress_before)
            .await
            .expect("productive recovery evaluates"),
        NonproductiveRecovery::NotEligible
    );
    assert!(matches!(
        fixture
            .store
            .recover_nonproductive_assignment(idle.assignment_id, no_progress_before)
            .await
            .expect("idle recovery evaluates"),
        NonproductiveRecovery::Recovered { .. }
    ));
}




#[tokio::test]
async fn exhausted_review_and_failed_verification_transition_to_needs_main() {
    let review_fixture = Fixture::new().await;
    let (review_worker, review_attempt) = review_fixture
        .store
        .create_assignment(
            review_fixture.repo.path(),
            worker_draft("review-root", "src"),
        )
        .await
        .expect("review worker");
    review_fixture
        .store
        .submit_agent_receipt_with_review(
            review_attempt.attempt_id,
            completed_receipt(Vec::new()),
            "cold review required".to_string(),
        )
        .await
        .expect("initial risk-gated receipt");
    let (_, reviewer_attempt) = review_fixture
        .store
        .create_assignment(
            review_fixture.repo.path(),
            relation_draft(
                "review-root",
                AgentRole::Reviewer,
                review_worker.assignment_id,
            ),
        )
        .await
        .expect("reviewer assignment");
    review_fixture
        .store
        .set_agent_gate(
            TaskActor::Attempt(reviewer_attempt.attempt_id),
            review_worker.assignment_id,
            GateKind::Review,
            GateStatus::ChangesRequested,
            "one correction is required".to_string(),
        )
        .await
        .expect("first review requests the bounded correction");
    let correction = review_fixture
        .store
        .amend_agent_task(
            TaskActor::Root,
            review_worker.assignment_id,
            AttemptAmendment {
                reason: "address the review finding".to_string(),
                objective: None,
                acceptance_criteria: None,
                stop_condition: None,
            },
        )
        .await
        .expect("single correction attempt");
    review_fixture
        .store
        .submit_agent_receipt_with_review(
            correction.attempt_id,
            completed_receipt(Vec::new()),
            "corrected work requires a fresh review".to_string(),
        )
        .await
        .expect("corrected receipt");
    review_fixture
        .store
        .set_agent_gate(
            TaskActor::Attempt(reviewer_attempt.attempt_id),
            review_worker.assignment_id,
            GateKind::Review,
            GateStatus::ChangesRequested,
            "the correction remains unresolved".to_string(),
        )
        .await
        .expect("second unresolved review becomes needs_main");
    let review_task = review_fixture
        .store
        .get_agent_task(review_worker.assignment_id, Some(10))
        .await
        .expect("review task");
    assert_eq!(review_task.current_attempt.state, AttemptState::NeedsMain);
    assert!(
        review_task
            .observations
            .iter()
            .any(|observation| observation.kind == ObservationKind::NeedsMain)
    );
    review_fixture
        .store
        .create_assignment(
            review_fixture.repo.path(),
            worker_draft("review-root", "src/file.rs"),
        )
        .await
        .expect("needs_main review releases the retained claim");

    let verification_fixture = Fixture::new().await;
    let (verification_worker, verification_attempt) = verification_fixture
        .store
        .create_assignment(
            verification_fixture.repo.path(),
            worker_draft("verification-root", "src"),
        )
        .await
        .expect("verification worker");
    verification_fixture
        .store
        .submit_agent_receipt_with_review(
            verification_attempt.attempt_id,
            completed_receipt(Vec::new()),
            "independent review and verification required".to_string(),
        )
        .await
        .expect("verification risk-gated receipt");
    let (_, verification_reviewer) = verification_fixture
        .store
        .create_assignment(
            verification_fixture.repo.path(),
            relation_draft(
                "verification-root",
                AgentRole::Reviewer,
                verification_worker.assignment_id,
            ),
        )
        .await
        .expect("verification reviewer");
    verification_fixture
        .store
        .set_agent_gate(
            TaskActor::Attempt(verification_reviewer.attempt_id),
            verification_worker.assignment_id,
            GateKind::Review,
            GateStatus::Passed,
            "cold review passed".to_string(),
        )
        .await
        .expect("review verdict");
    let (_, verifier_attempt) = verification_fixture
        .store
        .create_assignment(
            verification_fixture.repo.path(),
            relation_draft(
                "verification-root",
                AgentRole::Verifier,
                verification_worker.assignment_id,
            ),
        )
        .await
        .expect("verifier assignment");
    verification_fixture
        .store
        .set_agent_gate(
            TaskActor::Attempt(verifier_attempt.attempt_id),
            verification_worker.assignment_id,
            GateKind::Verification,
            GateStatus::Failed,
            "independent verification failed".to_string(),
        )
        .await
        .expect("failed verification becomes needs_main");
    let verification_task = verification_fixture
        .store
        .get_agent_task(verification_worker.assignment_id, Some(0))
        .await
        .expect("verification task");
    assert_eq!(
        verification_task.current_attempt.state,
        AttemptState::NeedsMain
    );
    verification_fixture
        .store
        .create_assignment(
            verification_fixture.repo.path(),
            worker_draft("verification-root", "src/file.rs"),
        )
        .await
        .expect("failed verification releases the retained claim");
}

#[tokio::test]
async fn wake_wait_is_event_driven_and_observes_the_next_commit() {
    let fixture = Fixture::new().await;
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("wait-root", "src"))
        .await
        .expect("assignment");
    let cursor = fixture
        .store
        .read_wake_events("wait-root".to_string(), None)
        .await
        .expect("initial wake read")
        .latest_event_id;
    let waiter_store = fixture.store.clone();
    let waiter = tokio::spawn(async move {
        waiter_store
            .wait_for_wake_events("wait-root".to_string(), cursor)
            .await
    });
    tokio::task::yield_now().await;

    fixture
        .store
        .append_observation(
            attempt.attempt_id,
            ObservationKind::Reading,
            "event-driven progress".to_string(),
            None,
        )
        .await
        .expect("observation appends");

    let wake = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .expect("wake wait should not poll until a maintenance boundary")
        .expect("wake task joins")
        .expect("wake read succeeds");
    assert_eq!(wake.updated_agents.len(), 1);
    assert_eq!(wake.updated_agents[0].reason, ObservationKind::Reading);
}

#[tokio::test]
async fn wake_wait_rejects_invalid_cursor_even_without_a_stream() {
    let fixture = Fixture::new().await;
    fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("cursor-owner", "src"))
        .await
        .expect("assignment");
    let cursor = fixture
        .store
        .read_wake_events("cursor-owner".into(), None)
        .await
        .expect("owner stream")
        .latest_event_id
        .expect("cursor");
    for cursor in [cursor, WakeEventId::new()] {
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.store.wait_for_wake_events("missing-root".into(), Some(cursor)),
        )
        .await
        .expect("invalid cursor must fail without waiting for a commit")
        .expect_err("cursor does not belong to missing root");
        assert!(matches!(error, StoreError::InvalidWakeWatermark(_)));
    }
    let empty = fixture
        .store
        .read_wake_events("missing-root".into(), None)
        .await
        .expect("cursorless empty read remains valid");
    assert_eq!(empty.status, WakeReadStatus::NoStream);
    assert!(empty.updated_agents.is_empty());
}

#[tokio::test]
async fn wake_wait_observes_a_commit_from_an_independent_store_instance() {
    let fixture = Fixture::new().await;
    let (_, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("external-wait-root", "src"),
        )
        .await
        .expect("assignment");
    let independent_store = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("independent store");
    let cursor = fixture
        .store
        .read_wake_events("external-wait-root".to_string(), None)
        .await
        .expect("initial wake read")
        .latest_event_id;
    let poll_count_before = fixture.store.durable_wake_poll_count();
    let mut waiters = Vec::new();
    for _ in 0..8 {
        let waiter_store = fixture.store.clone();
        waiters.push(tokio::spawn(async move {
            waiter_store
                .wait_for_wake_events("external-wait-root".to_string(), cursor)
                .await
        }));
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        fixture
            .store
            .wait_for_durable_wake_poll(waiters.len(), poll_count_before),
    )
    .await
    .expect("all waiters register and share a durable poll");
    let shared_poll_count = fixture
        .store
        .durable_wake_poll_count()
        .saturating_sub(poll_count_before);
    assert!(shared_poll_count > 0, "the shared durable poller runs");
    assert!(
        shared_poll_count < waiters.len() as u64,
        "concurrent waiters must share one durable database recheck poller"
    );

    independent_store
        .append_observation(
            attempt.attempt_id,
            ObservationKind::Reading,
            "cross-instance progress".to_string(),
            None,
        )
        .await
        .expect("independent observation appends");

    for waiter in waiters {
        let wake = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("durable wake recheck should observe the external commit")
            .expect("wake task joins")
            .expect("wake read succeeds");
        assert_eq!(wake.updated_agents.len(), 1);
        assert_eq!(wake.updated_agents[0].reason, ObservationKind::Reading);
    }
}

#[tokio::test]
async fn wake_stream_is_bounded_non_draining_and_rebuilt() {
    let fixture = Fixture::new().await;
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("wake-root", "src"))
        .await
        .expect("assignment");
    for index in 0..260 {
        fixture
            .store
            .append_observation(
                attempt.attempt_id,
                ObservationKind::Reading,
                format!("observation {index}"),
                None,
            )
            .await
            .expect("observation appends");
    }
    let first = fixture
        .store
        .read_wake_events("wake-root".to_string(), None)
        .await
        .expect("wake read");
    assert_eq!(first.updated_agents.len(), MAX_WAKE_EVENTS_PER_READ);
    // One acceptance event plus the 260 explicit observations precede this read.
    // A retained tail of MAX_WAKE_EVENTS_PER_ROOT therefore drops this many events.
    assert_eq!(
        first.lost_to_retention_count,
        (261 - MAX_WAKE_EVENTS_PER_ROOT) as u64
    );
    assert_eq!(
        first.remaining_count,
        (MAX_WAKE_EVENTS_PER_ROOT - MAX_WAKE_EVENTS_PER_READ) as u64
    );
    assert_eq!(
        first.truncated_count,
        first
            .lost_to_retention_count
            .saturating_add(first.remaining_count)
    );
    let watermark = first.updated_agents.last().expect("event").event_id;
    let repeated = fixture
        .store
        .read_wake_events("wake-root".to_string(), None)
        .await
        .expect("non-draining reread");
    assert_eq!(first.updated_agents, repeated.updated_agents);

    fixture.store.close().await;
    let pool = coordination_pool(&fixture).await;
    sqlx::query("DELETE FROM wake_events WHERE event_id = ?")
        .bind(watermark.to_string())
        .execute(&pool)
        .await
        .expect("derived wake event is removed to require repair");
    pool.close().await;
    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("restart reconstruction");
    let after = restarted
        .read_wake_events("wake-root".to_string(), Some(watermark))
        .await
        .expect("watermarked read after restart");
    assert!(!after.updated_agents.is_empty());
    assert_ne!(after.updated_agents[0].event_id, watermark);

    let mut cursor = None;
    let mut retained_ids = HashSet::new();
    let mut retained_events = 0;
    let mut retained_summaries = Vec::new();
    for _ in 0..10 {
        let page = restarted
            .read_wake_events("wake-root".to_string(), cursor)
            .await
            .expect("retained page reads");
        if page.updated_agents.is_empty() {
            assert_eq!(page.status, WakeReadStatus::Empty);
            assert!(!page.timed_out);
            break;
        }
        if cursor.is_none() {
            assert_eq!(page.lost_to_retention_count, first.lost_to_retention_count);
            assert_eq!(
                page.truncated_count,
                page.lost_to_retention_count
                    .saturating_add(page.remaining_count),
                "the initial retained page reports retention loss and unread events"
            );
        } else {
            assert_eq!(page.lost_to_retention_count, 0);
            assert_eq!(
                page.truncated_count, page.remaining_count,
                "watermarked pages must report only retained unread events"
            );
        }
        for event in &page.updated_agents {
            assert!(
                retained_ids.insert(event.event_id),
                "wake pagination must not duplicate events"
            );
            assert_eq!(event.attempt_id, attempt.attempt_id);
            assert_eq!(event.reason, ObservationKind::Reading);
            retained_summaries.push(event.summary.clone());
        }
        retained_events += page.updated_agents.len();
        cursor = page.latest_event_id;
    }
    assert_eq!(retained_events, MAX_WAKE_EVENTS_PER_ROOT);
    assert_eq!(retained_ids.len(), MAX_WAKE_EVENTS_PER_ROOT);
    assert_eq!(
        retained_summaries,
        (260 - MAX_WAKE_EVENTS_PER_ROOT..260)
            .map(|index| format!("observation {index}"))
            .collect::<Vec<_>>(),
        "reconstruction and pagination preserve the ordered tail of submitted observations"
    );
}

#[tokio::test]
async fn clean_restart_does_not_rebuild_current_wake_streams() {
    let fixture = Fixture::new().await;
    let (_, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("clean-restart-root", "src"),
        )
        .await
        .expect("assignment");
    fixture
        .store
        .append_observation(
            attempt.attempt_id,
            ObservationKind::Reading,
            "durable progress".to_string(),
            None,
        )
        .await
        .expect("observation appends");
    fixture.store.close().await;

    let pool = coordination_pool(&fixture).await;
    sqlx::query(
        "CREATE TRIGGER reject_clean_wake_event_rebuild
         BEFORE DELETE ON wake_events
         BEGIN SELECT RAISE(ABORT, 'clean wake events must not rebuild'); END",
    )
    .execute(&pool)
    .await
    .expect("wake event rebuild guard installs");
    sqlx::query(
        "CREATE TRIGGER reject_clean_wake_stream_rebuild
         BEFORE DELETE ON wake_streams
         BEGIN SELECT RAISE(ABORT, 'clean wake streams must not rebuild'); END",
    )
    .execute(&pool)
    .await
    .expect("wake stream rebuild guard installs");
    pool.close().await;

    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("clean restart skips derived wake rewrite");
    let wake = restarted
        .read_wake_events("clean-restart-root".to_string(), None)
        .await
        .expect("wake stream remains readable");
    assert_eq!(wake.updated_agents.len(), 2);
}

#[tokio::test]
async fn automatic_wake_cursor_is_consumer_scoped_bounded_and_compare_and_swap() {
    let fixture = Fixture::new().await;
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("cursor-root", "src"))
        .await
        .expect("assignment");
    for index in 0..260 {
        fixture
            .store
            .append_observation(
                attempt.attempt_id,
                ObservationKind::Reading,
                format!("cursor observation {index}"),
                None,
            )
            .await
            .expect("observation appends");
    }

    let consumer_a = fixture
        .store
        .automatic_wake_cursor("cursor-root".to_string(), "/root/a".to_string())
        .await
        .expect("automatic cursor initializes");
    let bounded = fixture
        .store
        .read_wake_events("cursor-root".to_string(), consumer_a)
        .await
        .expect("bounded snapshot reads");
    assert_eq!(bounded.updated_agents.len(), MAX_WAKE_EVENTS_PER_READ);
    let next = bounded.latest_event_id.expect("bounded snapshot watermark");
    assert!(
        fixture
            .store
            .compare_and_swap_automatic_wake_cursor(
                "cursor-root".to_string(),
                "/root/a".to_string(),
                consumer_a,
                next,
            )
            .await
            .expect("cursor advances")
    );
    assert!(
        !fixture
            .store
            .compare_and_swap_automatic_wake_cursor(
                "cursor-root".to_string(),
                "/root/a".to_string(),
                consumer_a,
                next,
            )
            .await
            .expect("stale cursor loses")
    );
    assert_eq!(
        fixture
            .store
            .automatic_wake_cursor("cursor-root".to_string(), "/root/a".to_string())
            .await
            .expect("advanced cursor reads"),
        Some(next)
    );

    let consumer_b = fixture
        .store
        .automatic_wake_cursor("cursor-root".to_string(), "/root/b".to_string())
        .await
        .expect("second consumer initializes independently");
    assert_eq!(consumer_b, consumer_a);
}

#[tokio::test]
async fn integrator_admission_does_not_depend_on_claim_overlap_or_supersession() {
    let fixture = Fixture::new().await;
    let (worker, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "shared"))
        .await
        .expect("worker assignment");
    let (untargeted, _) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "shared/file.rs"))
        .await
        .expect("untargeted overlapping claim is admitted as metadata");
    fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            worker.assignment_id,
            GateKind::Review,
            GateStatus::Pending,
            "cold review pending".to_string(),
        )
        .await
        .expect("pending gate");
    fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, completed_receipt(Vec::new()))
        .await
        .expect("successful dependency receipt");
    let mut integrator = worker_draft("root", "shared");
    integrator.role = AgentRole::Integrator;
    integrator.capability_profile = CapabilityProfile::IntegratorSourceWrite;
    integrator.dependencies = vec![worker.assignment_id];
    integrator.relation = Some(AssignmentRelation {
        kind: RelationKind::Integration,
        target_assignment_ids: vec![worker.assignment_id],
    });
    let blocked = fixture
        .store
        .create_assignment(fixture.repo.path(), integrator.clone())
        .await
        .expect_err("pending review gate must block a dependency");
    assert!(matches!(
        blocked,
        StoreError::DependencyBlocked { blockers }
            if blockers.iter().any(|blocker| {
                blocker.assignment_id == worker.assignment_id
                    && blocker.state == DependencyState::Incomplete
            })
    ));
    fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            worker.assignment_id,
            GateKind::Review,
            GateStatus::Passed,
            "cold review passed".to_string(),
        )
        .await
        .expect("passed gate");
    fixture
        .store
        .create_assignment(fixture.repo.path(), integrator)
        .await
        .expect("targeted integrator is admitted after the dependency gate passes");
    let unrelated = fixture.store.get_agent_task(untargeted.assignment_id, Some(0)).await.unwrap();
    assert_eq!(unrelated.current_attempt.state, AttemptState::Active);
    let pool = coordination_pool(&fixture).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM write_claims")
            .fetch_one(&pool).await.unwrap(),
        0
    );
    pool.close().await;
}














#[test]
fn assignments_without_additive_identity_or_capsule_fields_still_deserialize() {
    let repo = TempDir::new().expect("repository tempdir");
    let assignment = worker_draft("root", "src")
        .normalize(repo.path())
        .expect("assignment normalizes");
    let mut value = serde_json::to_value(assignment).expect("assignment serializes");
    value
        .as_object_mut()
        .expect("assignment object")
        .remove("repository_id");
    value
        .as_object_mut()
        .expect("assignment object")
        .remove("task_capsule");
    value
        .as_object_mut()
        .expect("assignment object")
        .remove("integration_plan");
    let decoded: Assignment = serde_json::from_value(value).expect("legacy assignment decodes");
    assert!(decoded.repository_id.is_empty());
    assert_eq!(decoded.task_capsule, None);
    assert_eq!(decoded.integration_plan, IntegrationPlan::SingleWriter);
}

#[tokio::test]
async fn task_capsule_attachment_is_canonical_and_one_time() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("capsule-root", "src"))
        .await
        .expect("worker assignment");
    let capsule = TaskCapsuleV1 {
        schema_version: 1,
        assignment_id: assignment.assignment_id,
        attempt_id: attempt.attempt_id,
        role: assignment.role,
        capability_profile: assignment.capability_profile,
        requirements: assignment.acceptance_criteria.clone(),
        objective: assignment.objective.clone(),
        read_scope: assignment.read_scope.clone(),
        write_scope: assignment.write_scope.clone(),
        stop_condition: assignment.stop_condition.clone(),
        dependencies: assignment.dependencies.clone(),
        risk_hints: assignment.risk_hints.clone(),
        contract_claims: assignment.contract_claims.clone(),
        workspace_strategy: Some(assignment.workspace_strategy),
        relation: assignment.relation.clone(),
        architecture_contract_ref: assignment.architecture_contract_ref.clone(),
        integration_plan: assignment.integration_plan,
        relevant_handles: vec![TaskCapsuleHandle::File {
            path: "src/new.rs".to_string(),
            existed: false,
            content_hash: None,
        }],
        workspace_epoch: assignment.start_epoch,
        workspace_manifest_hash: "manifest-sha256".to_string(),
        prohibited_changes: assignment.prohibited_changes.clone(),
        required_evidence: assignment.required_evidence.clone(),
    };
    assert_eq!(capsule.stop_condition, assignment.stop_condition);
    assert_eq!(capsule.dependencies, assignment.dependencies);
    assert_eq!(capsule.risk_hints, assignment.risk_hints);
    assert_eq!(capsule.contract_claims, assignment.contract_claims);
    assert_eq!(
        capsule.workspace_strategy,
        Some(assignment.workspace_strategy)
    );
    assert_eq!(capsule.relation, assignment.relation);
    assert_eq!(
        capsule.architecture_contract_ref,
        assignment.architecture_contract_ref
    );
    let canonical = serde_json::to_string(&capsule).expect("capsule serializes canonically");

    let attached = fixture
        .store
        .attach_task_capsule(
            assignment.assignment_id,
            attempt.attempt_id,
            canonical.clone(),
        )
        .await
        .expect("capsule attaches");
    assert_eq!(attached.task_capsule.as_deref(), Some(canonical.as_str()));
    assert_eq!(
        fixture
            .store
            .get_agent_task(assignment.assignment_id, None)
            .await
            .expect("task reloads")
            .assignment
            .task_capsule
            .as_deref(),
        Some(canonical.as_str())
    );
    assert!(matches!(
        fixture
            .store
            .attach_task_capsule(
                assignment.assignment_id,
                attempt.attempt_id,
                canonical,
            )
            .await,
        Err(StoreError::TaskCapsuleAlreadyAttached(id)) if id == assignment.assignment_id
    ));
}

#[tokio::test]
async fn task_capsule_attachment_rejects_noncanonical_or_mismatched_payloads() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("capsule-root", "src"))
        .await
        .expect("worker assignment");
    let capsule = TaskCapsuleV1 {
        schema_version: 1,
        assignment_id: assignment.assignment_id,
        attempt_id: attempt.attempt_id,
        role: assignment.role,
        capability_profile: assignment.capability_profile,
        requirements: assignment.acceptance_criteria.clone(),
        objective: assignment.objective.clone(),
        read_scope: assignment.read_scope.clone(),
        write_scope: assignment.write_scope.clone(),
        stop_condition: String::new(),
        dependencies: Vec::new(),
        risk_hints: Vec::new(),
        contract_claims: Vec::new(),
        workspace_strategy: None,
        relation: None,
        architecture_contract_ref: None,
        integration_plan: IntegrationPlan::SingleWriter,
        relevant_handles: Vec::new(),
        workspace_epoch: assignment.start_epoch,
        workspace_manifest_hash: "manifest-sha256".to_string(),
        prohibited_changes: assignment.prohibited_changes.clone(),
        required_evidence: assignment.required_evidence.clone(),
    };
    let legacy_canonical = serde_json::to_string(&capsule).expect("legacy capsule serializes");
    let legacy_value: serde_json::Value =
        serde_json::from_str(&legacy_canonical).expect("legacy capsule JSON parses");
    for field in [
        "stop_condition",
        "dependencies",
        "risk_hints",
        "contract_claims",
        "workspace_strategy",
        "relation",
        "architecture_contract_ref",
    ] {
        assert!(legacy_value.get(field).is_none(), "legacy field {field}");
    }
    let legacy_decoded: TaskCapsuleV1 =
        serde_json::from_str(&legacy_canonical).expect("legacy capsule decodes");
    assert_eq!(
        serde_json::to_string(&legacy_decoded).expect("legacy capsule reserializes"),
        legacy_canonical
    );
    let mut capsule_without_plan = legacy_value;
    capsule_without_plan
        .as_object_mut()
        .expect("legacy capsule object")
        .remove("integration_plan");
    assert_eq!(
        serde_json::from_value::<TaskCapsuleV1>(capsule_without_plan)
            .expect("pre-integration-plan capsule decodes")
            .integration_plan,
        IntegrationPlan::SingleWriter
    );
    let pretty = serde_json::to_string_pretty(&capsule).expect("capsule pretty serializes");
    assert!(matches!(
        fixture
            .store
            .attach_task_capsule(assignment.assignment_id, attempt.attempt_id, pretty)
            .await,
        Err(StoreError::InvalidTaskCapsule(_))
    ));

    let mut mismatched_plan = capsule.clone();
    mismatched_plan.integration_plan = IntegrationPlan::RootOwned;
    assert!(matches!(
        fixture
            .store
            .attach_task_capsule(
                assignment.assignment_id,
                attempt.attempt_id,
                serde_json::to_string(&mismatched_plan)
                    .expect("mismatched integration plan serializes"),
            )
            .await,
        Err(StoreError::InvalidTaskCapsule(_))
    ));

    let mut mismatched = capsule;
    mismatched.attempt_id = AttemptId::new();
    assert!(matches!(
        fixture
            .store
            .attach_task_capsule(
                assignment.assignment_id,
                attempt.attempt_id,
                serde_json::to_string(&mismatched).expect("mismatched capsule serializes"),
            )
            .await,
        Err(StoreError::InvalidTaskCapsule(_))
    ));
    let capsule_dir = fixture
        .state
        .codex_home()
        .join("agent-task-coordination")
        .join("task_capsules");
    assert!(
        std::fs::read_dir(&capsule_dir)
            .expect("capsule directory reads")
            .next()
            .is_none(),
        "rejected attachments must not leave published or staged capsules"
    );
    assert_eq!(
        fixture
            .store
            .get_agent_task(assignment.assignment_id, None)
            .await
            .expect("task remains readable after rejected attachments")
            .assignment
            .task_capsule,
        None
    );
    fixture
        .store
        .attach_task_capsule(
            assignment.assignment_id,
            attempt.attempt_id,
            legacy_canonical.clone(),
        )
        .await
        .expect("valid attachment succeeds after rejected inputs");
    assert_eq!(
        fixture
            .store
            .get_agent_task(assignment.assignment_id, None)
            .await
            .expect("consumer reads the valid attachment")
            .assignment
            .task_capsule
            .as_deref(),
        Some(legacy_canonical.as_str())
    );
}

#[tokio::test]
async fn correction_attempt_drift_updates_current_risk_gate() {
    let fixture = Fixture::new().await;
    let (worker, initial_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("drift-root", "src"))
        .await
        .expect("worker assignment");
    fixture
        .store
        .submit_agent_receipt_with_review(
            initial_attempt.attempt_id,
            completed_receipt(Vec::new()),
            "cold review required: missing successful focused validation".to_string(),
        )
        .await
        .expect("initial risk-gated receipt");
    let (_, reviewer_attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            relation_draft("drift-root", AgentRole::Reviewer, worker.assignment_id),
        )
        .await
        .expect("reviewer assignment");
    fixture
        .store
        .set_agent_gate(
            TaskActor::Attempt(reviewer_attempt.attempt_id),
            worker.assignment_id,
            GateKind::Review,
            GateStatus::ChangesRequested,
            "one correction is required".to_string(),
        )
        .await
        .expect("review requests correction");
    let correction = fixture
        .store
        .amend_agent_task(
            TaskActor::Root,
            worker.assignment_id,
            AttemptAmendment {
                reason: "address the review finding".to_string(),
                objective: None,
                acceptance_criteria: None,
                stop_condition: None,
            },
        )
        .await
        .expect("single correction attempt");
    fixture
        .store
        .submit_agent_receipt_with_review(
            correction.attempt_id,
            completed_receipt(Vec::new()),
            format!("cold review required: {CONCURRENT_DRIFT_REASON}"),
        )
        .await
        .expect("correction receipt with observed drift");

    let task = fixture
        .store
        .get_agent_task(worker.assignment_id, Some(10))
        .await
        .expect("task with correction receipt");
    let risk_gate = task
        .gates
        .iter()
        .find(|gate| gate.kind == GateKind::Risk)
        .expect("current risk gate");
    assert_eq!(
        risk_gate.reason,
        format!(
            "cold review required: missing successful focused validation; {CONCURRENT_DRIFT_REASON}"
        )
    );
}

#[test]
fn risk_gate_and_waiver_rules_are_deterministic() {
    let facts = RiskFacts {
        domains: BTreeSet::from([RiskDomain::Persistence]),
        non_generated_changed_files: 6,
        non_generated_changed_lines: 401,
        focused_validation_succeeded: false,
        ..RiskFacts::default()
    };
    let decision = evaluate_risk_gate(&facts);
    assert!(decision.review_required);
    assert_eq!(decision.reasons, vec![
        "persistence risk",
        "more than five non-generated changed files",
        "more than 400 non-generated changed lines",
        "missing successful focused validation",
    ]);
    assert!(GateKind::Review.is_waivable());
    assert!(GateKind::Verification.is_waivable());
    assert!(!GateKind::Mutation.is_waivable());
    assert!(!GateKind::Ownership.is_waivable());
    assert!(!GateKind::Risk.is_waivable());
    let boundary = evaluate_risk_gate(&RiskFacts {
        non_generated_changed_files: 5,
        non_generated_changed_lines: 400,
        focused_validation_succeeded: true,
        ..RiskFacts::default()
    });
    assert!(!boundary.review_required);
    assert!(boundary.reasons.is_empty());
    let decision = evaluate_risk_gate(&RiskFacts {
        focused_validation_succeeded: true,
        drift: true,
        ..RiskFacts::default()
    });

    assert_eq!(decision.reasons, vec![CONCURRENT_DRIFT_REASON.to_string()]);
}




#[tokio::test]
async fn bounded_validation_operation_suspends_only_until_its_hard_deadline() {
    let fixture = Fixture::new().await;
    let command = "cargo test -p owner bounded-operation";
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft("bounded-operation-root", "src/lib.rs", command),
        )
        .await
        .expect("bounded-operation assignment");
    let running = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "bounded-operation-call",
        command,
    )
    .await;
    let deadline = running
        .evidence
        .lease_expires_at
        .expect("bounded operation has a hard deadline");
    let before_deadline = deadline - Duration::seconds(1);
    let suspended = crate::local::with_test_comparison_now(before_deadline, async {
        fixture
            .store
            .reserve_stalled_nudge(
                assignment.assignment_id,
                before_deadline - Duration::seconds(60),
            )
            .await
    })
    .await
    .expect("pre-deadline productivity check");
    assert!(
        !suspended,
        "a live bounded operation suspends idle recovery"
    );
    let recovery = crate::local::with_test_comparison_now(before_deadline, async {
        fixture
            .store
            .recover_nonproductive_assignment(
                assignment.assignment_id,
                before_deadline - Duration::seconds(60),
            )
            .await
    })
    .await
    .expect("pre-deadline recovery evaluation");
    assert_eq!(
        recovery,
        NonproductiveRecovery::Suspended(ProductivitySummary {
            active_owned_operation_count: 1,
            cancelled_expired_operation_count: 0,
            recovery_threshold_seconds: 60,
            recovery_policy_version: NONPRODUCTIVE_RECOVERY_POLICY_VERSION,
        })
    );

    let after_deadline = deadline + Duration::seconds(121);
    let recovery = crate::local::with_test_comparison_now(after_deadline, async {
        fixture
            .store
            .recover_nonproductive_assignment(
                assignment.assignment_id,
                after_deadline - Duration::seconds(120),
            )
            .await
    })
    .await
    .expect("post-deadline productivity recovery");
    let NonproductiveRecovery::Recovered {
        receipt,
        productivity,
    } = recovery
    else {
        panic!("expired operation should no longer suppress recovery: {recovery:?}");
    };
    assert_eq!(receipt.status, AgentStatusClaim::Abandoned);
    assert_eq!(productivity.cancelled_expired_operation_count, 1);
    assert_eq!(
        fixture
            .store
            .get_validation_call(running.call_id)
            .await
            .expect("cancelled operation reads")
            .expect("cancelled operation remains durable")
            .status,
        ValidationCallStatus::Cancelled
    );
}


#[tokio::test]
async fn nudge_leases_and_task_restart_are_durable() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "before\n").expect("lib fixture");
    let command = "cargo test -p owner restart";
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft("restart-root", "src/lib.rs", command),
        )
        .await
        .expect("restart assignment");
    let initial_wakes = fixture
        .store
        .read_wake_events("restart-root".to_string(), None)
        .await
        .expect("initial wake read");
    let cursor = initial_wakes.latest_event_id.expect("initial cursor");
    assert!(
        fixture
            .store
            .reserve_stalled_nudge(assignment.assignment_id, Utc::now() + Duration::seconds(1))
            .await
            .expect("first nudge reserves")
    );
    assert!(
        !fixture
            .store
            .reserve_stalled_nudge(assignment.assignment_id, Utc::now() + Duration::seconds(1))
            .await
            .expect("duplicate nudge checks")
    );
    fixture
        .store
        .append_observation(
            attempt.attempt_id,
            ObservationKind::Reading,
            "fresh progress".to_string(),
            None,
        )
        .await
        .expect("progress resets nudge");
    assert!(
        fixture
            .store
            .reserve_stalled_nudge(assignment.assignment_id, Utc::now() + Duration::seconds(1))
            .await
            .expect("new no-progress period may nudge once")
    );
    let call = finish_focused_validation(
        &fixture.store,
        start_focused_validation(
            &fixture.store,
            attempt.attempt_id,
            "restart-validation",
            command,
        )
        .await,
    )
    .await;
    assert!(call.evidence.lease_expires_at.is_none());
    fixture.store.close().await;
    std::fs::write(
        fixture.repo.path().join("src/lib.rs"),
        "changed during restart\n",
    )
    .expect("restart drift");
    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("store reconstructs");
    let wakes = restarted
        .read_wake_events("restart-root".to_string(), Some(cursor))
        .await
        .expect("wake cursor reconstructs");
    assert!(
        wakes
            .updated_agents
            .iter()
            .any(|event| event.reason == ObservationKind::Reading)
    );
    let task = restarted
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("restarted task reads");
    assert_eq!(task.workspace_status.lease_state, Some(LeaseState::Active));
    restarted
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt(vec!["restart-validation".to_string()]),
        )
        .await
        .expect("validation history remains usable without workspace freshness tracking");
    restarted.close().await;
}

#[tokio::test]
async fn json_timestamps_order_validation_calls_and_bindings_by_instant() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft(
                "timestamp-order-root",
                "validation",
                "legacy ordering probe",
            ),
        )
        .await
        .expect("validation ordering assignment");
    let timestamp_variants = [
        ("order-d", "2099-01-01T00:00:00Z"),
        ("order-a", "2099-01-01T00:00:00.000Z"),
        ("order-c", "2099-01-01T00:00:00.000000Z"),
        ("order-b", "2099-01-01T00:00:00.000000000Z"),
    ];
    for (call_id, _) in timestamp_variants {
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: call_id.to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "legacy ordering probe".to_string(),
                evidence: ValidationEvidence::default(),
                status: ValidationCallStatus::Running,
                recorded_at: fixed_time("2099-01-01T00:00:00Z"),
            })
            .await
            .expect("ordering validation call starts");
    }

    let pool = coordination_pool(&fixture).await;
    for (call_id, timestamp) in timestamp_variants {
        sqlx::query("UPDATE validation_calls SET recorded_at = ? WHERE call_id = ?")
            .bind(json_time(timestamp))
            .bind(call_id)
            .execute(&pool)
            .await
            .expect("validation timestamp width updates");
    }
    let validation_ids = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("ordered validation task reads")
        .validation_calls
        .into_iter()
        .map(|call| call.call_id)
        .collect::<Vec<_>>();
    assert_eq!(
        validation_ids,
        vec!["order-a", "order-b", "order-c", "order-d"]
    );

    let binding_variants = [
        ("/root/order-d", "2099-01-01T00:00:00Z"),
        ("/root/order-a", "2099-01-01T00:00:00.000Z"),
        ("/root/order-c", "2099-01-01T00:00:00.000000Z"),
        ("/root/order-b", "2099-01-01T00:00:00.000000000Z"),
    ];
    for (index, (agent_path, timestamp)) in binding_variants.into_iter().enumerate() {
        let (binding_assignment, binding_attempt) = fixture
            .store
            .create_assignment(
                fixture.repo.path(),
                worker_draft("timestamp-binding-root", &format!("binding/{index}")),
            )
            .await
            .expect("binding ordering assignment");
        fixture
            .store
            .bind_agent_task(AgentTaskBindingDraft {
                assignment_id: binding_assignment.assignment_id,
                attempt_id: binding_attempt.attempt_id,
                agent_path: agent_path.to_string(),
                task_name: format!("order-{index}"),
                thread_id: Some(format!("order-thread-{index}")),
            })
            .await
            .expect("ordering binding persists");
        sqlx::query("UPDATE agent_task_bindings SET updated_at = ? WHERE assignment_id = ?")
            .bind(json_time(timestamp))
            .bind(binding_assignment.assignment_id.to_string())
            .execute(&pool)
            .await
            .expect("binding timestamp width updates");
    }
    pool.close().await;

    let binding_paths = fixture
        .store
        .list_agent_task_bindings("timestamp-binding-root".to_string(), None)
        .await
        .expect("ordered bindings read")
        .into_iter()
        .map(|binding| binding.agent_path)
        .collect::<Vec<_>>();
    assert_eq!(
        binding_paths,
        vec![
            "/root/order-a",
            "/root/order-b",
            "/root/order-c",
            "/root/order-d"
        ]
    );
}

#[tokio::test]
async fn json_timestamp_comparisons_cover_mixed_precision_boundaries() {
    let fixture = Fixture::new().await;
    let mut first_draft = worker_draft("timestamp-independent-root", "independent/first");
    first_draft.required_evidence = vec!["focused test".to_string()];
    let (_, first_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), first_draft)
        .await
        .expect("first independent assignment");
    let mut second_draft = worker_draft("timestamp-independent-root", "independent/second");
    second_draft.required_evidence = vec!["focused test".to_string()];
    let (second_assignment, second_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), second_draft)
        .await
        .expect("second independent assignment");
    let comparison_now = fixed_time("2099-01-01T00:00:00.001Z");
    crate::local::with_test_comparison_now(
        comparison_now,
        fixture.store.record_validation_call(ValidationCall {
            call_id: "fraction-first".to_string(),
            attempt_id: first_attempt.attempt_id,
            command_summary: "focused test".to_string(),
            evidence: ValidationEvidence {
                lease_expires_at: Some(fixed_time("2099-01-01T00:00:00Z")),
                ..ValidationEvidence::default()
            },
            status: ValidationCallStatus::Running,
            recorded_at: comparison_now,
        }),
    )
    .await
    .expect("first validation starts");
    crate::local::with_test_comparison_now(
        comparison_now,
        fixture.store.record_validation_call(ValidationCall {
            call_id: "fraction-second".to_string(),
            attempt_id: second_attempt.attempt_id,
            command_summary: "focused test".to_string(),
            evidence: ValidationEvidence::default(),
            status: ValidationCallStatus::Running,
            recorded_at: comparison_now,
        }),
    )
    .await
    .expect("second validation starts independently");
    let second = fixture
        .store
        .get_agent_task(second_assignment.assignment_id, Some(0))
        .await
        .expect("successor task reads")
        .validation_calls
        .into_iter()
        .find(|call| call.call_id == "fraction-second")
        .expect("second validation call exists");
    assert_eq!(second.attempt_id, second_attempt.attempt_id);

    let (nudge_assignment, nudge_attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("timestamp-nudge-root", "nudge"),
        )
        .await
        .expect("nudge assignment");
    let pool = coordination_pool(&fixture).await;
    sqlx::query(
        "UPDATE workspace_actors
         SET state = 'active', last_progress_at = ?, nudge_sent_at = NULL
         WHERE attempt_id = ?",
    )
    .bind(json_time("2099-01-01T00:00:00.100000Z"))
    .bind(nudge_attempt.attempt_id.to_string())
    .execute(&pool)
    .await
    .expect("six-digit nudge timestamp updates");
    pool.close().await;
    let nudge_boundary = fixed_time("2099-01-01T00:00:00.100000001Z");
    assert!(
        crate::local::with_test_comparison_now(
            nudge_boundary,
            fixture
                .store
                .reserve_stalled_nudge(nudge_assignment.assignment_id, nudge_boundary),
        )
        .await
        .expect("nine-digit nudge boundary evaluates")
    );
}

#[tokio::test]
async fn legacy_repository_bindings_upgrade_to_lineage_ids_on_restart() {
    let fixture = Fixture::new().await;
    std::fs::create_dir(fixture.repo.path().join(".git")).expect("git marker");
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("source directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "before\n").expect("source file");
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("legacy-upgrade-root", "src/lib.rs"),
        )
        .await
        .expect("current assignment");
    let lineage_id =
        repository_lineage_id(fixture.repo.path()).expect("current repository lineage");
    assert_ne!(lineage_id, assignment.workspace_id);
    fixture.store.close().await;

    let database_path = fixture
        .state
        .codex_home()
        .join("agent-task-coordination")
        .join("agent_tasks.sqlite");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(database_path)
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("legacy database opens");
    sqlx::query("DROP TRIGGER assignments_immutable_update")
        .execute(&pool)
        .await
        .expect("assignment trigger drops for legacy fixture");
    sqlx::query("DROP TRIGGER assignment_repositories_immutable_update")
        .execute(&pool)
        .await
        .expect("binding trigger drops for legacy fixture");
    let mut legacy_body = serde_json::to_value(&assignment).expect("assignment serializes");
    let legacy_object = legacy_body
        .as_object_mut()
        .expect("assignment body is an object");
    legacy_object.insert(
        "repository_id".to_string(),
        serde_json::Value::String(assignment.workspace_id.clone()),
    );
    legacy_object.remove("workspace_id");
    sqlx::query("UPDATE assignments SET body_json = ? WHERE assignment_id = ?")
        .bind(serde_json::to_string(&legacy_body).expect("legacy body serializes"))
        .bind(assignment.assignment_id.to_string())
        .execute(&pool)
        .await
        .expect("legacy assignment body");
    sqlx::query("UPDATE assignment_repositories SET repository_id = ? WHERE assignment_id = ?")
        .bind(&assignment.workspace_id)
        .bind(assignment.assignment_id.to_string())
        .execute(&pool)
        .await
        .expect("legacy binding identity");
    sqlx::query("UPDATE workspace_repositories SET repository_id = ? WHERE workspace_id = ?")
        .bind(&assignment.workspace_id)
        .bind(&assignment.workspace_id)
        .execute(&pool)
        .await
        .expect("legacy workspace lineage");
    sqlx::query(
        "CREATE TRIGGER assignments_immutable_update
         BEFORE UPDATE ON assignments
         BEGIN
             SELECT RAISE(ABORT, 'assignments are immutable');
         END",
    )
    .execute(&pool)
    .await
    .expect("assignment trigger restores");
    sqlx::query(
        "CREATE TRIGGER assignment_repositories_immutable_update
         BEFORE UPDATE ON assignment_repositories
         WHEN OLD.assignment_id <> NEW.assignment_id
           OR OLD.canonical_root <> NEW.canonical_root
           OR OLD.bound_at <> NEW.bound_at
           OR OLD.workspace_id <> NEW.workspace_id
           OR OLD.repository_id <> OLD.workspace_id
         BEGIN
             SELECT RAISE(ABORT, 'assignment repository bindings are immutable');
         END",
    )
    .execute(&pool)
    .await
    .expect("binding trigger restores");
    pool.close().await;

    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("legacy store reopens");
    let task = restarted
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("upgraded task reads");
    assert_eq!(task.assignment.repository_id, lineage_id);
    assert_eq!(task.assignment.workspace_id, assignment.workspace_id);
    assert_eq!(task.current_attempt.attempt_id, attempt.attempt_id);
    restarted.close().await;
}


#[tokio::test]
async fn malformed_git_lineage_metadata_rejects_assignment_without_persisting_fallback_identity() {
    for invalid in [
        "marker",
        "missing_gitdir",
        "empty_commondir",
        "unreadable_commondir",
    ] {
        let fixture = Fixture::new().await;
        let root = fixture.repo.path();
        std::fs::create_dir(root.join("src")).expect("source directory");
        std::fs::write(root.join("src/lib.rs"), "before\n").expect("source file");
        let git_dir = root.join(".git-metadata");
        let common_dir = root.join(".git-common");
        std::fs::create_dir(&common_dir).expect("common Git directory");
        if invalid != "missing_gitdir" {
            std::fs::create_dir(&git_dir).expect("worktree Git directory");
        }
        std::fs::write(
            root.join(".git"),
            if invalid == "marker" {
                "not a Git directory marker\n"
            } else {
                "gitdir: .git-metadata\n"
            },
        )
        .expect("Git marker");
        match invalid {
            "empty_commondir" => {
                std::fs::write(git_dir.join("commondir"), " \n").expect("empty commondir")
            }
            "unreadable_commondir" => {
                std::fs::write(git_dir.join("commondir"), [0xff]).expect("invalid UTF-8 commondir")
            }
            _ => {}
        }
        let error = fixture
            .store
            .create_assignment(root, worker_draft("invalid-lineage", "src/lib.rs"))
            .await
            .expect_err("invalid Git metadata must reject admission");
        assert!(
            matches!(error, StoreError::InvalidScope(_)),
            "{invalid}: {error}"
        );
        let pool = coordination_pool(&fixture).await;
        for (table, query) in [
            ("assignments", "SELECT COUNT(*) FROM assignments"),
            (
                "assignment_repositories",
                "SELECT COUNT(*) FROM assignment_repositories",
            ),
            (
                "workspace_repositories",
                "SELECT COUNT(*) FROM workspace_repositories",
            ),
        ] {
            let count: i64 = sqlx::query_scalar(query)
                .fetch_one(&pool)
                .await
                .expect("persisted identity count");
            assert_eq!(count, 0, "{invalid}: rejected admission wrote {table}");
        }
        pool.close().await;
        std::fs::create_dir_all(&git_dir).expect("repair worktree Git directory");
        std::fs::write(root.join(".git"), "gitdir: .git-metadata\n").expect("repair Git marker");
        std::fs::write(git_dir.join("commondir"), "../.git-common\n")
            .expect("repair common directory");
        let other_workspace = TempDir::new().expect("linked peer workspace");
        std::fs::write(
            other_workspace.path().join(".git"),
            format!("gitdir: {}\n", common_dir.display()),
        )
        .expect("peer Git marker");
        let peer_lineage = repository_lineage_id(other_workspace.path()).expect("peer lineage");
        let (assignment, _) = fixture
            .store
            .create_assignment(root, worker_draft("repaired-lineage", "src/lib.rs"))
            .await
            .expect("repaired metadata admits assignment");
        assert_eq!(
            assignment.repository_id, peer_lineage,
            "linked workspaces share coordination lineage"
        );
        assert_ne!(
            assignment.repository_id, assignment.workspace_id,
            "Git lineage must not fall back to checkout identity"
        );
    }
}

#[tokio::test]
async fn attempt_ordinal_migration_initializes_fresh_and_upgrades_existing_stores() {
    for predecessor in [false, true] {
        let mut prior_attempt_triggers = Vec::new();
        let codex_home = TempDir::new().expect("codex home tempdir");
        let repo = TempDir::new().expect("repository tempdir");
        let state =
            StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
                .await
                .expect("state initializes");
        let coordination_root = state.codex_home().join("agent-task-coordination");
        tokio::fs::create_dir_all(&coordination_root)
            .await
            .expect("coordination directory creates");
        if predecessor {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::new()
                        .filename(coordination_root.join("agent_tasks.sqlite"))
                        .create_if_missing(true)
                        .foreign_keys(true),
                )
                .await
                .expect("predecessor database opens");
            task_store_migrator_through(18)
                .run(&pool)
                .await
                .expect("prior schema applies");
            prior_attempt_triggers = sqlx::query_as::<_, (String, String)>(
                "SELECT name, sql FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'attempts' ORDER BY name",
            ).fetch_all(&pool).await.unwrap();
            // These opaque payloads must survive byte-for-byte. No migration
            // may reinterpret a sealed receipt or a prior amendment.
            sqlx::raw_sql(
                "INSERT INTO assignments VALUES ('prior-assignment', 'prior-root', '{}', 'created');
                 INSERT INTO attempts VALUES
                   ('prior-zero', 'prior-assignment', 0, NULL, 'completed', 'created-zero', 'sealed-zero'),
                   ('prior-one', 'prior-assignment', 1, '{\"reason\":\"retained\"}', 'completed', 'created-one', 'sealed-one');
                 INSERT INTO receipts VALUES
                   ('prior-zero', 'prior-assignment', 'completed', '{\"summary\":\"original\"}', 'sealed-zero');
                 INSERT INTO agent_task_bindings VALUES
                   ('prior-assignment', 'prior-one', 'prior-root', '/root/prior', 'prior', 'prior-thread', 'bound', 'updated');",
            )
            .execute(&pool)
            .await
            .expect("predecessor attempts, receipt, and binding seed");
            assert!(
                sqlx::query("INSERT INTO attempts VALUES ('too-early', 'prior-assignment', 2, NULL, 'active', 'created', NULL)")
                    .execute(&pool)
                    .await
                    .is_err(),
                "the predecessor must reproduce the ordinal constraint"
            );
            pool.close().await;
        }
        // Exercise the normal startup path and its embedded migration set.
        let store = LocalAgentTaskStore::initialize(&state)
            .await
            .expect("normal initialization applies the ordinal migration");
        let fixture = Fixture {
            _codex_home: codex_home,
            repo,
            state,
            store,
        };
        let pool = coordination_pool(&fixture).await;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 19 AND success = 1"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
        if predecessor {
            assert_eq!(
                sqlx::query_as::<_, (String, String)>(
                    "SELECT name, sql FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'attempts' ORDER BY name",
                ).fetch_all(&pool).await.unwrap(),
                prior_attempt_triggers,
                "the migration preserves every live attempts trigger"
            );
            let attempts =
                sqlx::query_as::<_, (String, i64, Option<String>, String, String, Option<String>)>(
                    "SELECT attempt_id, ordinal, amendment_json, state, created_at, sealed_at
                 FROM attempts WHERE assignment_id = 'prior-assignment' ORDER BY ordinal",
                )
                .fetch_all(&pool)
                .await
                .unwrap();
            assert_eq!(
                attempts,
                vec![
                    (
                        "prior-zero".into(),
                        0,
                        None,
                        "completed".into(),
                        "created-zero".into(),
                        Some("sealed-zero".into())
                    ),
                    (
                        "prior-one".into(),
                        1,
                        Some("{\"reason\":\"retained\"}".into()),
                        "completed".into(),
                        "created-one".into(),
                        Some("sealed-one".into())
                    ),
                ]
            );
            assert_eq!(
                sqlx::query_as::<_, (String, String, String)>("SELECT attempt_id, body_json, sealed_at FROM receipts WHERE assignment_id = 'prior-assignment'")
                    .fetch_one(&pool).await.unwrap(),
                ("prior-zero".into(), "{\"summary\":\"original\"}".into(), "sealed-zero".into())
            );
            assert_eq!(
                sqlx::query_as::<_, (String, String, String, String, String)>("SELECT attempt_id, agent_path, thread_id, bound_at, updated_at FROM agent_task_bindings WHERE assignment_id = 'prior-assignment'")
                    .fetch_one(&pool).await.unwrap(),
                ("prior-one".into(), "/root/prior".into(), "prior-thread".into(), "bound".into(), "updated".into())
            );
        }
        let (assignment, _) = fixture
            .store
            .create_assignment(
                fixture.repo.path(),
                worker_draft("post-migration-root", "src"),
            )
            .await
            .expect("normal assignment creation still works");
        for ordinal in [1, 2, 255] {
            sqlx::query("INSERT INTO attempts VALUES (?, ?, ?, NULL, 'active', 'created', NULL)")
                .bind(format!("supported-{ordinal}"))
                .bind(assignment.assignment_id.to_string())
                .bind(ordinal)
                .execute(&pool)
                .await
                .expect("supported ordinal inserts");
        }
        for ordinal in [-1, 256] {
            let error = sqlx::query(
                "INSERT INTO attempts VALUES (?, ?, ?, NULL, 'active', 'created', NULL)",
            )
            .bind(format!("unsupported-{ordinal}"))
            .bind(assignment.assignment_id.to_string())
            .bind(ordinal)
            .execute(&pool)
            .await
            .expect_err("out-of-range ordinals stay rejected");
            assert!(
                error.to_string().contains("CHECK constraint failed"),
                "{error}"
            );
        }
        assert!(
            sqlx::query(
                "INSERT INTO attempts VALUES ('duplicate', ?, 2, NULL, 'active', 'created', NULL)"
            )
            .bind(assignment.assignment_id.to_string())
            .execute(&pool)
            .await
            .is_err(),
            "assignment ordinals remain unique"
        );
        assert!(sqlx::query("INSERT INTO attempts VALUES ('fractional', ?, 2.5, NULL, 'active', 'created', NULL)")
            .bind(assignment.assignment_id.to_string()).execute(&pool).await.is_err(), "ordinals remain integers");
        assert!(sqlx::query("INSERT INTO attempts VALUES ('orphan', 'missing-assignment', 2, NULL, 'active', 'created', NULL)")
            .execute(&pool).await.is_err(), "attempt ownership still requires a parent");
        assert!(
            sqlx::query("UPDATE attempts SET ordinal = 3 WHERE attempt_id = 'supported-2'")
                .execute(&pool)
                .await
                .is_err(),
            "amendment immutability remains enforced"
        );
        assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'attempts_assignment_ordinal_idx'")
            .fetch_one(&pool).await.unwrap(), 1);
        assert!(
            sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&pool)
                .await
                .unwrap()
                .is_empty()
        );
        pool.close().await;
        fixture.store.close().await;
    }
}

#[tokio::test]
async fn automatic_wake_delivery_is_atomic_with_cursor_and_survives_losing_publish() {
    let fixture = Fixture::new().await;
    let root = "delivery-root".to_string();
    let consumer = "/root/consumer".to_string();
    let (_, attempt) = fixture.store.create_assignment(fixture.repo.path(), worker_draft(&root, "src")).await.unwrap();
    let cursor = fixture.store.automatic_wake_cursor(root.clone(), consumer.clone()).await.unwrap();
    fixture.store.append_observation(attempt.attempt_id, ObservationKind::Reading, "ready".to_string(), None).await.unwrap();
    let batch = fixture.store.read_wake_events(root.clone(), cursor).await.unwrap();
    let next = batch.latest_event_id.unwrap();
    assert!(fixture.store.automatic_wake_delivery(root.clone(), consumer.clone()).await.unwrap().is_none());
    let receipt = r#"{"artifact_id":"retained-output","cursor":1}"#.to_string();
    assert!(fixture.store.publish_automatic_wake_delivery(root.clone(), consumer.clone(), cursor, next, receipt.clone()).await.unwrap());
    assert!(!fixture.store.publish_automatic_wake_delivery(root.clone(), consumer.clone(), cursor, next, "losing-output".to_string()).await.unwrap());
    assert_eq!(fixture.store.automatic_wake_cursor(root.clone(), consumer.clone()).await.unwrap(), Some(next));
    assert_eq!(fixture.store.automatic_wake_delivery(root, consumer).await.unwrap(), Some(receipt));
}

#[tokio::test]
async fn reusable_explorer_lookup_matches_admission_without_creating_work() {
    let fixture = Fixture::new().await;
    let draft = explorer_draft("early-reuse-root", "src/file.rs", "trace parser ownership");
    assert!(fixture.store.reusable_explorer_assignment(fixture.repo.path(), draft.clone()).await.unwrap().is_none());
    let admitted = fixture.store.create_admitted_assignment(fixture.repo.path(), draft.clone(), true).await.unwrap();
    assert_eq!(fixture.store.reusable_explorer_assignment(fixture.repo.path(), draft.clone()).await.unwrap(), Some(admitted.assignment.assignment_id));
    let distinct = explorer_draft("early-reuse-root", "src/file.rs", "trace serializer ownership");
    assert!(fixture.store.reusable_explorer_assignment(fixture.repo.path(), distinct).await.unwrap().is_none());
    fixture.store.submit_agent_receipt(admitted.attempt.attempt_id, completed_receipt(Vec::new())).await.unwrap();
    assert!(fixture.store.reusable_explorer_assignment(fixture.repo.path(), draft).await.unwrap().is_none());
}

#[tokio::test]
#[ignore = "manual wall-clock benchmark; no timing assertion"]
async fn benchmark_explorer_lookup_with_sealed_history() {
    let fixture = Fixture::new().await;
    let mut draft = explorer_draft("history-benchmark", "src", "new investigation");
    draft.acceptance_criteria = (0..64)
        .map(|index| AcceptanceCriterion {
            id: format!("criterion-{index:03}"),
            text: format!("inspect the complete contract and consumer for requirement {index}"),
        })
        .collect();
    let mut historical_draft = draft.clone();
    historical_draft.objective = "prior investigation".to_string();
    let (template, _) = fixture
        .store
        .create_assignment(fixture.repo.path(), historical_draft)
        .await
        .expect("active assignment creates");
    let canonical_root = std::fs::canonicalize(fixture.repo.path()).unwrap();
    let pool = coordination_pool(&fixture).await;
    let mut transaction = pool.begin().await.unwrap();
    let now = serde_json::to_string(&Utc::now()).unwrap();
    for _ in 0..512 {
        let mut assignment = template.clone();
        assignment.assignment_id = AssignmentId::new();
        let id = assignment.assignment_id.to_string();
        sqlx::query("INSERT INTO assignments VALUES (?, ?, ?, ?)")
            .bind(&id)
            .bind(&assignment.root_session_id)
            .bind(serde_json::to_string(&assignment).unwrap())
            .bind(&now)
            .execute(&mut *transaction)
            .await
            .unwrap();
        sqlx::query("INSERT INTO assignment_repositories (assignment_id, repository_id, canonical_root, bound_at, workspace_id) VALUES (?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(&assignment.repository_id)
            .bind(canonical_root.to_string_lossy().as_ref())
            .bind(&now)
            .bind(&assignment.workspace_id)
            .execute(&mut *transaction)
            .await
            .unwrap();
        sqlx::query("INSERT INTO attempts VALUES (?, ?, 0, NULL, '\"completed\"', ?, ?)")
            .bind(AttemptId::new().to_string())
            .bind(&id)
            .bind(&now)
            .bind(&now)
            .execute(&mut *transaction)
            .await
            .unwrap();
    }
    transaction.commit().await.unwrap();
    let mut samples = Vec::new();
    for iteration in 0..10 {
        let started = std::time::Instant::now();
        let result = fixture
            .store
            .reusable_explorer_assignment(fixture.repo.path(), draft.clone())
            .await
            .expect("lookup reads complete history");
        let elapsed = started.elapsed();
        assert_eq!(result, None, "sealed history cannot establish reuse");
        if iteration > 0 {
            samples.push(elapsed);
        }
    }
    samples.sort();
    eprintln!(
        "explorer lookup: 512 sealed / 1 active, 64 criteria, median {:?}",
        samples[4]
    );
    pool.close().await;
    fixture.store.close().await;
}
