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
use crate::workspace::TestWorkspaceCapturePause;
use crate::workspace::with_test_workspace_capture_pause;

#[test]
fn editing_and_tool_calls_are_meaningful_progress() {
    assert!(ObservationKind::Editing.is_meaningful_progress());
    assert!(ObservationKind::ToolCall.is_meaningful_progress());
    assert!(!ObservationKind::Starting.is_meaningful_progress());
}

#[tokio::test]
async fn audit_mutation_recovery_f060_page_reports_completeness_and_rejects_zero() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src creates");
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("audit-page", "src"))
        .await
        .expect("assignment creates");
    for path in ["src/a.rs", "src/b.rs"] {
        std::fs::write(fixture.repo.path().join(path), "before").expect("file writes");
        fixture
            .store
            .begin_mutation(
                attempt.attempt_id,
                fixture.repo.path(),
                path.to_string(),
                AttributionConfidence::Definitive,
            )
            .await
            .expect("mutation begins");
    }
    let (page, query_count) = fixture
        .store
        .list_mutation_evidence_page_with_query_count(attempt.attempt_id, Some(1))
        .await
        .expect("page reads");
    assert_eq!(query_count, 2);
    assert_eq!(page.evidence.len(), 1);
    assert_eq!(page.total_count, 2);
    assert!(page.truncated);
    assert_eq!(page.next_cursor, Some(1));
    assert!(matches!(
        fixture
            .store
            .list_mutation_evidence(attempt.attempt_id, Some(0))
            .await,
        Err(StoreError::InvalidMutationEvidenceLimit(0))
    ));
}

#[tokio::test]
async fn audit_mutation_recovery_f061_finalize_pending_is_atomic() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src creates");
    let (_, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("audit-finalize-all", "src"),
        )
        .await
        .expect("assignment creates");
    for path in ["src/a.rs", "src/b.rs"] {
        std::fs::write(fixture.repo.path().join(path), "before").expect("file writes");
        fixture
            .store
            .begin_mutation(
                attempt.attempt_id,
                fixture.repo.path(),
                path.into(),
                AttributionConfidence::Definitive,
            )
            .await
            .expect("mutation begins");
    }
    std::fs::remove_file(fixture.repo.path().join("src/b.rs")).expect("file removes");
    std::fs::create_dir(fixture.repo.path().join("src/b.rs")).expect("directory replaces file");
    assert!(
        fixture
            .store
            .finalize_pending_mutations(attempt.attempt_id)
            .await
            .is_err()
    );
    let evidence = fixture
        .store
        .list_mutation_evidence(attempt.attempt_id, None)
        .await
        .expect("evidence reads");
    assert!(evidence.iter().all(|item| item.finalized_at.is_none()));
}

#[tokio::test]
async fn audit_mutation_recovery_f062_prewrite_capture_holds_coordination_lock() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src creates");
    std::fs::write(fixture.repo.path().join("src/a.rs"), "before").expect("file writes");
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("audit-pre", "src"))
        .await
        .expect("assignment creates");
    let pause = Arc::new(TestSnapshotCapturePause::new());
    let task_pause = Arc::clone(&pause);
    let store = fixture.store.clone();
    let repo = fixture.repo.path().to_path_buf();
    let task = tokio::spawn(async move {
        with_test_snapshot_capture_pause(
            task_pause,
            store.begin_mutation(
                attempt.attempt_id,
                &repo,
                "src/a.rs".into(),
                AttributionConfidence::Definitive,
            ),
        )
        .await
    });
    let permit = pause.started.acquire().await.expect("capture pauses");
    permit.forget();
    let task_read = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        fixture
            .store
            .get_agent_task(assignment.assignment_id, Some(0)),
    )
    .await
    .expect("an unrelated task read must not wait for snapshot capture")
    .expect("task read succeeds");
    assert_eq!(task_read.assignment.assignment_id, assignment.assignment_id);
    let pool = coordination_pool(&fixture).await;
    let mut connection = pool.acquire().await.expect("connection opens");
    let writer = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection),
    )
    .await;
    assert!(
        !matches!(writer, Ok(Ok(_))),
        "writer must remain excluded during prewrite capture"
    );
    pause.release.add_permits(1);
    task.await.expect("task joins").expect("mutation begins");
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
async fn audit_mutation_recovery_f063_final_snapshot_matches_commit_evidence() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src creates");
    std::fs::write(fixture.repo.path().join("src/a.rs"), "before").expect("file writes");
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("audit-final", "src"))
        .await
        .expect("assignment creates");
    fixture
        .store
        .begin_mutation(
            attempt.attempt_id,
            fixture.repo.path(),
            "src/a.rs".into(),
            AttributionConfidence::Definitive,
        )
        .await
        .expect("mutation begins");
    std::fs::write(fixture.repo.path().join("src/a.rs"), "committed").expect("file mutates");
    let pause = Arc::new(TestSnapshotCapturePause::new());
    let task_pause = Arc::clone(&pause);
    let store = fixture.store.clone();
    let repo = fixture.repo.path().to_path_buf();
    let task = tokio::spawn(async move {
        with_test_snapshot_capture_pause(
            task_pause,
            store.finalize_mutation(attempt.attempt_id, &repo, "src/a.rs".into()),
        )
        .await
    });
    let permit = pause.started.acquire().await.expect("capture pauses");
    permit.forget();
    let pool = coordination_pool(&fixture).await;
    let mut connection = pool.acquire().await.expect("connection opens");
    let writer = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection),
    )
    .await;
    assert!(
        !matches!(writer, Ok(Ok(_))),
        "writer must remain excluded until final evidence commits"
    );
    pause.release.add_permits(1);
    let evidence = task.await.expect("task joins").expect("mutation finalizes");
    let snapshot = fixture
        .store
        .read_mutation_snapshot(
            attempt.attempt_id,
            "src/a.rs".into(),
            MutationSnapshotVersion::Final,
            0,
            None,
        )
        .await
        .expect("snapshot reads");
    assert_eq!(snapshot.bytes, b"committed");
    assert!(evidence.end_epoch.is_some());
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

#[tokio::test]
async fn audit_mutation_recovery_f067_epoch_overflow_is_rejected() {
    let fixture = Fixture::new().await;
    assert!(matches!(
        fixture.store.read_workspace_events(fixture.repo.path(), u64::MAX).await,
        Err(StoreError::CorruptData(message)) if message.contains("SQLite integer range")
    ));
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
        .attach_task_capsule(assignment.assignment_id, attempt.attempt_id, canonical)
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
    assert!(
        restarted
            .get_agent_task(assignment.assignment_id, None)
            .await
            .expect("task reads")
            .assignment
            .task_capsule
            .is_some()
    );
    restarted.close().await;
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
async fn audit_workspace_actor_reset_clears_binding_and_lease() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("scope directory");
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("reset-root", "src"))
        .await
        .expect("assignment creates");
    let registration = WorkspaceActorRegistration {
        root_session_id: "reset-root".to_string(),
        actor_id: "reset-actor".to_string(),
        kind: WorkspaceActorKind::Typed,
        assignment_id: Some(assignment.assignment_id),
        attempt_id: Some(attempt.attempt_id),
        strategy: WorkspaceStrategy::Shared,
    };
    fixture
        .store
        .register_workspace_actor(fixture.repo.path(), registration)
        .await
        .expect("bound actor registers");
    let pool = coordination_pool(&fixture).await;
    sqlx::query(
        "UPDATE workspace_actors SET state = 'active', lease_expires_at = ? WHERE actor_id = ?",
    )
    .bind(serde_json::to_string(&(Utc::now() + Duration::hours(1))).expect("lease serializes"))
    .bind("reset-actor")
    .execute(&pool)
    .await
    .expect("actor simulates active lease");

    fixture
        .store
        .register_workspace_actor(
            fixture.repo.path(),
            WorkspaceActorRegistration {
                root_session_id: "reset-root".to_string(),
                actor_id: "reset-actor".to_string(),
                kind: WorkspaceActorKind::Typed,
                assignment_id: None,
                attempt_id: None,
                strategy: WorkspaceStrategy::Shared,
            },
        )
        .await
        .expect("actor resets");
    let row = sqlx::query_as::<_, (Option<String>, Option<String>, String, Option<String>)>(
        "SELECT assignment_id, attempt_id, state, lease_expires_at FROM workspace_actors WHERE actor_id = ?",
    )
    .bind("reset-actor")
    .fetch_one(&pool)
    .await
    .expect("reset actor reads");
    assert_eq!(row, (None, None, "idle".to_string(), None));
}

#[tokio::test]
async fn audit_workspace_capture_publish_order_matches_scan_order() {
    let fixture = Fixture::new().await;
    let path = fixture.repo.path().join("ordered.txt");
    std::fs::write(&path, "old").expect("initial file");
    fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["ordered.txt".to_string()])
        .await
        .expect("baseline captures");

    let pause = Arc::new(TestWorkspaceCapturePause::new());
    let first_store = fixture.store.clone();
    let first_root = fixture.repo.path().to_path_buf();
    let first_pause = Arc::clone(&pause);
    let first = tokio::spawn(async move {
        with_test_workspace_capture_pause(first_pause, async move {
            first_store
                .capture_workspace_revision(&first_root, vec!["ordered.txt".to_string()])
                .await
        })
        .await
    });
    let started = tokio::time::timeout(std::time::Duration::from_secs(1), pause.started.acquire())
        .await
        .expect("first scan reaches pause")
        .expect("pause remains open");
    started.forget();
    std::fs::write(&path, "new").expect("file changes between captures");
    let second_store = fixture.store.clone();
    let second_root = fixture.repo.path().to_path_buf();
    let second = tokio::spawn(async move {
        second_store
            .capture_workspace_revision(&second_root, vec!["ordered.txt".to_string()])
            .await
    });
    pause.release.add_permits(1);
    let first = first
        .await
        .expect("first task joins")
        .expect("first capture");
    let second = second
        .await
        .expect("second task joins")
        .expect("second capture");
    assert!(second.epoch > first.epoch);
    assert_ne!(second.manifest_hash, first.manifest_hash);
}

#[tokio::test]
async fn audit_workspace_git_capture_includes_tracked_generated_named_paths() {
    let fixture = Fixture::new().await;
    run_git(fixture.repo.path(), &["init", "--quiet"]);
    std::fs::create_dir_all(fixture.repo.path().join("build")).expect("build directory");
    std::fs::write(fixture.repo.path().join("build/source.rs"), "old").expect("tracked file");
    run_git(fixture.repo.path(), &["add", "build/source.rs"]);
    run_git(
        fixture.repo.path(),
        &[
            "-c",
            "user.name=KD4 Audit",
            "-c",
            "user.email=kd4-audit@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    std::fs::write(fixture.repo.path().join("build/source.rs"), "new").expect("tracked change");
    let revision = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec![REPOSITORY_WIDE_PATH.to_string()])
        .await
        .expect("Git overlay captures");
    assert_eq!(revision.capture_mode, WorkspaceCaptureMode::GitOverlay);
    assert!(revision.complete);
    assert!(revision.discovery_errors.is_empty());
    assert!(
        revision
            .files
            .iter()
            .any(|entry| entry.path == "build/source.rs")
    );
}

#[tokio::test]
async fn audit_workspace_fallback_is_incomplete_and_rejected_for_validation() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.repo.path().join("input.txt"), "input").expect("fallback input");
    let revision = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec![REPOSITORY_WIDE_PATH.to_string()])
        .await
        .expect("fallback captures diagnostically");
    assert_eq!(
        revision.capture_mode,
        WorkspaceCaptureMode::FilesystemFallback
    );
    assert!(!revision.complete);
    assert!(!revision.discovery_errors.is_empty());
    assert!(crate::local::require_complete_workspace_capture(&revision).is_err());
}

fn create_audit_symlink(target: &std::path::Path, link: &std::path::Path) {
    std::os::windows::fs::symlink_file(target, link).expect("file symlink creates");
}

#[tokio::test]
async fn audit_workspace_symlink_target_identity_affects_manifest() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.repo.path().join("left.txt"), "same").expect("left target");
    std::fs::write(fixture.repo.path().join("right.txt"), "same").expect("right target");
    let link = fixture.repo.path().join("link.txt");
    create_audit_symlink(std::path::Path::new("left.txt"), &link);
    let left = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["link.txt".to_string()])
        .await
        .expect("left link captures");
    std::fs::remove_file(&link).expect("left link removes");
    create_audit_symlink(std::path::Path::new("right.txt"), &link);
    let right = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["link.txt".to_string()])
        .await
        .expect("right link captures");
    assert_ne!(left.files, right.files);
    assert_ne!(left.manifest_hash, right.manifest_hash);
}

#[tokio::test]
async fn audit_workspace_broken_symlink_differs_from_absent_path() {
    let fixture = Fixture::new().await;
    let link = fixture.repo.path().join("link.txt");
    create_audit_symlink(std::path::Path::new("missing.txt"), &link);
    let broken = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["link.txt".to_string()])
        .await
        .expect("broken link captures");
    std::fs::remove_file(&link).expect("broken link removes");
    let absent = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["link.txt".to_string()])
        .await
        .expect("absent path captures");
    assert!(broken.files[0].existed);
    assert!(!absent.files[0].existed);
    assert_ne!(broken.manifest_hash, absent.manifest_hash);
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
            recorded_at,
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
        let codex_home = TempDir::new().expect("codex home tempdir");
        let repo = TempDir::new().expect("repository tempdir");
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

async fn assert_writer_blocked_while_snapshot_capture_is_paused(
    pause: &TestSnapshotCapturePause,
    blocker_pool: &sqlx::SqlitePool,
) {
    let permit = tokio::time::timeout(std::time::Duration::from_secs(1), pause.started.acquire())
        .await
        .expect("snapshot capture reaches the test pause")
        .expect("snapshot pause remains open");
    permit.forget();

    let mut blocker = blocker_pool
        .acquire()
        .await
        .expect("independent coordination connection opens");
    let writer_result = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *blocker),
    )
    .await;
    let writer_acquired = matches!(&writer_result, Ok(Ok(_)));
    if writer_acquired {
        sqlx::query("ROLLBACK")
            .execute(&mut *blocker)
            .await
            .expect("independent writer lock is released");
    }
    pause.release.add_permits(1);
    assert!(
        !writer_acquired,
        "snapshot capture must retain the SQLite writer transaction"
    );
}

#[tokio::test]
async fn workspace_actor_registration_waits_for_transient_writer_contention() {
    let fixture = Fixture::new().await;
    assert_eq!(
        fixture
            .store
            .configured_busy_timeout_millis()
            .await
            .expect("busy timeout reads"),
        30_000,
    );
    let blocker_pool = coordination_pool(&fixture).await;
    let mut blocker = blocker_pool
        .acquire()
        .await
        .expect("coordination connection opens");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *blocker)
        .await
        .expect("writer lock is acquired");

    let registration = fixture.store.register_workspace_actor(
        fixture.repo.path(),
        WorkspaceActorRegistration {
            root_session_id: "contended-root".to_string(),
            actor_id: "contended-reader".to_string(),
            kind: WorkspaceActorKind::Root,
            assignment_id: None,
            attempt_id: None,
            strategy: WorkspaceStrategy::Shared,
        },
    );
    tokio::pin!(registration);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut registration)
            .await
            .is_err(),
        "actor registration should wait while another connection owns the writer lock"
    );

    sqlx::query("ROLLBACK")
        .execute(&mut *blocker)
        .await
        .expect("writer lock is released");
    tokio::time::timeout(std::time::Duration::from_secs(1), registration)
        .await
        .expect("registration resumes promptly after the writer lock is released")
        .expect("registration survives transient writer contention");
    drop(blocker);
    blocker_pool.close().await;
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
    let events = store
        .read_workspace_events(repo.path(), 0)
        .await
        .expect("retained workspace events read through production API");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].paths, vec!["src/lib.rs".to_string()]);
    assert_eq!(events[0].contracts, vec!["stable-contract".to_string()]);

    let fixture = Fixture {
        _codex_home: codex_home,
        repo,
        state,
        store,
    };
    let pool = coordination_pool(&fixture).await;
    let remaining = sqlx::query_as::<_, (String, String)>(
        "SELECT type, name
         FROM sqlite_master
         WHERE (type = 'table' AND name IN (
                    'actor_supporting_reads',
                    'workspace_manifest_payloads',
                    'workspace_mutation_leases',
                    'workspace_finalization_fences'
                ))
            OR (type = 'trigger' AND name LIKE 'finalization_blocks_%')
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

async fn remove_workspace_actor(fixture: &Fixture, attempt_id: AttemptId) {
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
    let deleted = sqlx::query("DELETE FROM workspace_actors WHERE attempt_id = ?")
        .bind(attempt_id.to_string())
        .execute(&pool)
        .await
        .expect("workspace actor is removed");
    assert_eq!(deleted.rows_affected(), 1);
    pool.close().await;
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
    let receipt = fixture
        .store
        .submit_agent_receipt(
            architect_attempt.attempt_id,
            ReceiptDraft {
                status: AgentStatusClaim::Completed,
                summary: "architecture sealed".to_string(),
                criterion_results: vec![CriterionResult {
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
        .expect("architect receipt");
    let sealed = receipt
        .architecture_contract
        .expect("sealed architecture contract");
    assert_eq!(sealed.contract.schema_version, 1);
    assert_eq!(sealed.contract_sha256.len(), 64);

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

fn completed_receipt_with_changes(
    validation_call_ids: Vec<String>,
    paths: &[&str],
) -> ReceiptDraft {
    let mut receipt = completed_receipt(validation_call_ids);
    receipt.declared_changes = paths
        .iter()
        .map(|path| DeclaredChange {
            path: (*path).to_string(),
            summary: "versioned controlled change".to_string(),
        })
        .collect();
    receipt
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
    store
        .capture_workspace_revision(repo_root, vec![path.to_string()])
        .await
        .expect("pre-write workspace revision captures");
    store
        .begin_mutation(
            attempt_id,
            repo_root,
            path.to_string(),
            AttributionConfidence::Definitive,
        )
        .await
        .expect("typed mutation evidence starts");
    std::fs::write(repo_root.join(path), contents).expect("controlled file write");
    store
        .capture_workspace_revision(repo_root, vec![path.to_string()])
        .await
        .expect("post-write workspace revision captures");
    let evidence = store
        .finalize_mutation(attempt_id, repo_root, path.to_string())
        .await
        .expect("typed mutation evidence finalizes");
    assert_eq!(evidence.assignment_id, assignment_id);
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
    let mut draft = worker_draft("root", "src");
    draft.role = AgentRole::Reviewer;
    draft.capability_profile = CapabilityProfile::ReadSearchDiff;
    draft.dependencies = vec![target];
    draft.relation = Some(AssignmentRelation {
        kind: RelationKind::Review,
        target_assignment_ids: vec![target],
    });
    assert!(draft.clone().normalize(repo.path()).is_err());
    draft.write_scope.clear();
    assert!(draft.normalize(repo.path()).is_ok());
}

#[tokio::test]
async fn selective_admission_keeps_overlapping_claims_as_metadata() {
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
        4
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM contract_claims WHERE active = 1 AND contract_name = 'shared-api'",
        )
        .fetch_one(&pool)
        .await
        .expect("active contract claim count reads"),
        3
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
        .expect_err("the sealed result is reused instead of spawning duplicate work");
    assert!(matches!(
        completed_duplicate,
        StoreError::AdmissionRejected {
            reason: AdmissionRejectionReason::DuplicateExplorerInvestigation,
            reusable_assignment_id: Some(assignment_id),
        }
        if assignment_id == first.assignment.assignment_id
    ));
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
async fn completed_receipt_rejects_workspace_change_after_validation() {
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
    let error = fixture
        .store
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt(vec!["fresh-receipt-call".to_string()]),
        )
        .await
        .expect_err("stale validation cannot seal a completed receipt");
    assert!(matches!(
        error,
        StoreError::EvidenceSuperseded { call_ids }
            if call_ids == vec!["fresh-receipt-call".to_string()]
    ));

    let task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("unsealed task reloads");
    assert!(task.receipt.is_none());
    assert_eq!(task.current_attempt.state, AttemptState::Active);
    let pool = coordination_pool(&fixture).await;
    let active_claims = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM write_claims WHERE assignment_id = ? AND active = 1",
    )
    .bind(assignment.assignment_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("active claim count reads");
    assert_eq!(active_claims, 1);
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
    let quiescence = fixture
        .store
        .check_quiescence("receipt-seal-root".to_string())
        .await
        .expect("terminal quiescence reads");
    assert!(quiescence.quiescent);
    assert!(quiescence.running_validation_call_ids.is_empty());
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
            .begin_mutation(
                attempt.attempt_id,
                fixture.repo.path(),
                "src/repaired.rs".to_string(),
                AttributionConfidence::Definitive,
            )
            .await,
        Err(StoreError::AttemptNotActive(_))
    ));
    fixture
        .store
        .begin_mutation(
            correction.attempt_id,
            fixture.repo.path(),
            "src/repaired.rs".to_string(),
            AttributionConfidence::Definitive,
        )
        .await
        .expect("correction mutation evidence starts");
    assert!(matches!(
        fixture
            .store
            .amend_agent_task(TaskActor::Root, assignment.assignment_id, amendment)
            .await,
        Err(StoreError::AmendmentLimitReached(_))
    ));
}

#[tokio::test]
async fn risk_review_progresses_to_independent_verification_without_releasing_claim() {
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
    let gated_quiescence = fixture
        .store
        .check_quiescence("risk-root".to_string())
        .await
        .expect("risk-gated quiescence reads");
    assert!(
        gated_quiescence
            .active_claim_assignment_ids
            .contains(&worker.assignment_id),
        "the pending review keeps claim metadata active"
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
    let pool = coordination_pool(&fixture).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT active FROM write_claims WHERE assignment_id = ?",)
            .bind(worker.assignment_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("reviewed claim metadata reads"),
        0,
        "claim metadata releases only after verification passes"
    );
    pool.close().await;
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
                .heartbeat_typed_workspace_actor(binding.clone()),
        )
        .await
        .expect("typed heartbeat")
    );
    let mut mismatched = binding.clone();
    mismatched.thread_id = Some("wrong-thread".to_string());
    assert!(
        !crate::local::with_test_comparison_now(
            comparison_now,
            fixture.store.heartbeat_typed_workspace_actor(mismatched),
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
            fixture.store.heartbeat_typed_workspace_actor(binding),
        )
        .await
        .expect("sealed heartbeat is rejected")
    );
}

#[tokio::test]
async fn orphaned_owner_claims_release_after_the_liveness_window() {
    let expired_fixture = Fixture::new().await;
    let (expired_assignment, expired_attempt) = expired_fixture
        .store
        .create_assignment(
            expired_fixture.repo.path(),
            worker_draft("expired-owner-root", "src"),
        )
        .await
        .expect("expired-owner assignment");
    let comparison_now =
        expire_workspace_actor_leases(&expired_fixture, &[expired_attempt.attempt_id]).await;
    crate::local::with_test_comparison_now(
        comparison_now,
        expired_fixture
            .store
            .check_quiescence("expired-owner-root".to_string()),
    )
    .await
    .expect("quiescence scavenges the expired owner");
    let expired_task = expired_fixture
        .store
        .get_agent_task(expired_assignment.assignment_id, Some(10))
        .await
        .expect("expired owner task reads");
    assert_eq!(expired_task.current_attempt.state, AttemptState::NeedsMain);
    assert!(expired_task.observations.iter().any(|observation| {
        observation.kind == ObservationKind::NeedsMain
            && observation.summary.contains("owner lease expired")
    }));

    let missing_fixture = Fixture::new().await;
    let (missing_assignment, missing_attempt) = missing_fixture
        .store
        .create_assignment(
            missing_fixture.repo.path(),
            worker_draft("missing-owner-root", "src"),
        )
        .await
        .expect("missing-owner assignment");
    remove_workspace_actor(&missing_fixture, missing_attempt.attempt_id).await;
    missing_fixture
        .store
        .check_quiescence("missing-owner-root".to_string())
        .await
        .expect("quiescence scavenges a claim without an owner record");
    assert_eq!(
        missing_fixture
            .store
            .get_agent_task(missing_assignment.assignment_id, Some(0))
            .await
            .expect("missing owner task reads")
            .current_attempt
            .state,
        AttemptState::NeedsMain
    );
}

#[tokio::test]
async fn claimless_typed_actor_is_recovered_after_its_liveness_window() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            explorer_draft("claimless-owner-root", "src", "inspect the bounded source"),
        )
        .await
        .expect("claimless explorer assignment");
    let pool = coordination_pool(&fixture).await;
    let active_claim_count = sqlx::query_scalar::<_, i64>(
        "SELECT
             (SELECT COUNT(*) FROM write_claims WHERE assignment_id = ? AND active = 1)
           + (SELECT COUNT(*) FROM contract_claims WHERE assignment_id = ? AND active = 1)",
    )
    .bind(assignment.assignment_id.to_string())
    .bind(assignment.assignment_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("claim metadata reads");
    assert_eq!(active_claim_count, 0, "explorer is intentionally claimless");
    pool.close().await;

    let comparison_now = expire_workspace_actor_leases(&fixture, &[attempt.attempt_id]).await;
    let quiescence = crate::local::with_test_comparison_now(
        comparison_now,
        fixture
            .store
            .check_quiescence("claimless-owner-root".to_string()),
    )
    .await
    .expect("quiescence recovers the expired claimless actor");
    assert!(quiescence.quiescent);
    assert!(quiescence.active_assignment_ids.is_empty());

    let task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(10))
        .await
        .expect("recovered explorer task reads");
    assert_eq!(task.current_attempt.state, AttemptState::NeedsMain);
    assert!(task.observations.iter().any(|observation| {
        observation.kind == ObservationKind::NeedsMain
            && observation.summary.contains("workspace actor recovered")
    }));
}

#[tokio::test]
async fn live_reviewer_preserves_a_gated_claim_after_the_worker_lease_expires() {
    let fixture = Fixture::new().await;
    let (worker, worker_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("review-root", "src"))
        .await
        .expect("worker assignment");
    fixture
        .store
        .submit_agent_receipt_with_review(
            worker_attempt.attempt_id,
            completed_receipt(Vec::new()),
            "cold review required".to_string(),
        )
        .await
        .expect("review-gated receipt");
    let (_, reviewer_attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            relation_draft("review-root", AgentRole::Reviewer, worker.assignment_id),
        )
        .await
        .expect("reviewer assignment");

    let worker_comparison_now =
        expire_workspace_actor_leases(&fixture, &[worker_attempt.attempt_id]).await;
    let live_relation = crate::local::with_test_comparison_now(
        worker_comparison_now,
        fixture.store.check_quiescence("review-root".to_string()),
    )
    .await
    .expect("live related reviewer is considered");
    assert!(
        live_relation
            .active_claim_assignment_ids
            .contains(&worker.assignment_id)
    );

    let reviewer_comparison_now =
        expire_workspace_actor_leases(&fixture, &[reviewer_attempt.attempt_id]).await;
    let released = crate::local::with_test_comparison_now(
        reviewer_comparison_now,
        fixture.store.check_quiescence("review-root".to_string()),
    )
    .await
    .expect("stale related actors are scavenged");
    assert!(
        !released
            .active_claim_assignment_ids
            .contains(&worker.assignment_id)
    );
    fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("competing-root", "src/file.rs"),
        )
        .await
        .expect("claim releases after both the owner and related reviewer become stale");
    let task = fixture
        .store
        .get_agent_task(worker.assignment_id, Some(10))
        .await
        .expect("orphaned gated task reads");
    assert_eq!(task.current_attempt.state, AttemptState::NeedsMain);
    assert!(
        task.gates
            .iter()
            .any(|gate| gate.kind == GateKind::Review && gate.status == GateStatus::Pending)
    );
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
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
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
    assert!(first.truncated_count > 0);
    assert!(first.lost_to_retention_count > 0);
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
        }
        retained_events += page.updated_agents.len();
        cursor = page.latest_event_id;
    }
    assert_eq!(retained_events, MAX_WAKE_EVENTS_PER_ROOT);
    assert_eq!(retained_ids.len(), MAX_WAKE_EVENTS_PER_ROOT);
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
    let pool = coordination_pool(&fixture).await;
    let targeted = sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT active, superseded_by FROM write_claims WHERE assignment_id = ?",
    )
    .bind(worker.assignment_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("targeted claim reads");
    assert_eq!(targeted, (0, None));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT active FROM write_claims WHERE assignment_id = ?",)
            .bind(untargeted.assignment_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("untargeted claim reads"),
        1
    );
    pool.close().await;
}

#[tokio::test]
async fn write_claims_and_mutations_are_bound_to_exact_repositories() {
    let fixture = Fixture::new().await;
    let other_repo = TempDir::new().expect("second repository tempdir");
    let (_, first_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "shared"))
        .await
        .expect("first repository claim");
    fixture
        .store
        .create_assignment(other_repo.path(), worker_draft("root", "shared"))
        .await
        .expect("same relative scope in another repository does not conflict");
    assert!(matches!(
        fixture
            .store
            .begin_mutation(
                first_attempt.attempt_id,
                other_repo.path(),
                "shared/file.rs".to_string(),
                AttributionConfidence::Definitive,
            )
            .await,
        Err(StoreError::RepositoryMismatch(_))
    ));
}

#[tokio::test]
async fn mutation_evidence_and_receipts_are_not_gated_by_claim_metadata() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.repo.path().join("outside.txt"), "before\n")
        .expect("outside-scope fixture");
    let (_, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("evidence-without-claim-root", "src/claimed.rs"),
        )
        .await
        .expect("claimed assignment is admitted");

    fixture
        .store
        .begin_mutation(
            attempt.attempt_id,
            fixture.repo.path(),
            "outside.txt".to_string(),
            AttributionConfidence::Definitive,
        )
        .await
        .expect("evidence outside claim metadata starts");
    std::fs::write(fixture.repo.path().join("outside.txt"), "after\n")
        .expect("outside-scope mutation");
    fixture
        .store
        .finalize_mutation(
            attempt.attempt_id,
            fixture.repo.path(),
            "outside.txt".to_string(),
        )
        .await
        .expect("evidence outside claim metadata finalizes");
    let receipt = fixture
        .store
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt_with_changes(Vec::new(), &["outside.txt"]),
        )
        .await
        .expect("receipt outside claim metadata seals");
    assert_eq!(receipt.status, AgentStatusClaim::Completed);
}

#[tokio::test]
async fn mutation_evidence_keeps_private_prewrite_snapshot() {
    let fixture = Fixture::new().await;
    tokio::fs::create_dir_all(fixture.repo.path().join("src"))
        .await
        .expect("source directory");
    tokio::fs::write(fixture.repo.path().join("src/file.rs"), b"before")
        .await
        .expect("prewrite file");
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "src"))
        .await
        .expect("worker assignment");
    let attempt_id = attempt.attempt_id;
    let blocker_pool = coordination_pool(&fixture).await;
    let begin_pause = Arc::new(TestSnapshotCapturePause::new());
    let begin_store = fixture.store.clone();
    let begin_repo = fixture.repo.path().to_path_buf();
    let begin_pause_scope = Arc::clone(&begin_pause);
    let begin = tokio::spawn(async move {
        with_test_snapshot_capture_pause(begin_pause_scope, async move {
            begin_store
                .begin_mutation(
                    attempt_id,
                    &begin_repo,
                    "src/file.rs".to_string(),
                    AttributionConfidence::Definitive,
                )
                .await
        })
        .await
    });
    assert_writer_blocked_while_snapshot_capture_is_paused(&begin_pause, &blocker_pool).await;
    let event_id = begin
        .await
        .expect("begin mutation task joins")
        .expect("mutation begins");
    tokio::fs::write(fixture.repo.path().join("src/file.rs"), b"after")
        .await
        .expect("mutated file");
    let finalize_pause = Arc::new(TestSnapshotCapturePause::new());
    let finalize_store = fixture.store.clone();
    let finalize_repo = fixture.repo.path().to_path_buf();
    let finalize_pause_scope = Arc::clone(&finalize_pause);
    let finalize = tokio::spawn(async move {
        with_test_snapshot_capture_pause(finalize_pause_scope, async move {
            finalize_store
                .finalize_mutation(attempt_id, &finalize_repo, "src/file.rs".to_string())
                .await
        })
        .await
    });
    assert_writer_blocked_while_snapshot_capture_is_paused(&finalize_pause, &blocker_pool).await;
    let evidence = finalize
        .await
        .expect("finalize mutation task joins")
        .expect("mutation finalizes");
    assert_eq!(evidence.mutation_event_ids, vec![event_id]);
    assert_ne!(evidence.pre_write_hash, evidence.final_hash);
    assert!(evidence.snapshot_retained);
    tokio::fs::write(
        fixture.repo.path().join("src/file.rs"),
        b"later live contents",
    )
    .await
    .expect("live file changes after evidence finalization");
    let pre_first = fixture
        .store
        .read_mutation_snapshot(
            attempt.attempt_id,
            "src/file.rs".to_string(),
            MutationSnapshotVersion::PreWrite,
            0,
            Some(2),
        )
        .await
        .expect("first prewrite snapshot chunk");
    assert_eq!(pre_first.bytes, b"be");
    assert_eq!(pre_first.total_bytes, 6);
    let pre_rest = fixture
        .store
        .read_mutation_snapshot(
            attempt.attempt_id,
            "src/file.rs".to_string(),
            MutationSnapshotVersion::PreWrite,
            pre_first.next_offset.expect("prewrite continuation"),
            Some(16),
        )
        .await
        .expect("remaining prewrite snapshot chunk");
    assert_eq!(pre_rest.bytes, b"fore");
    assert_eq!(pre_rest.next_offset, None);
    let final_snapshot = fixture
        .store
        .read_mutation_snapshot(
            attempt.attempt_id,
            "src/file.rs".to_string(),
            MutationSnapshotVersion::Final,
            0,
            None,
        )
        .await
        .expect("final snapshot remains stable");
    assert_eq!(final_snapshot.bytes, b"after");
    assert_eq!(
        fixture
            .store
            .list_mutation_evidence(attempt.attempt_id, Some(1))
            .await
            .expect("bounded evidence list"),
        vec![evidence.clone()]
    );
    assert!(matches!(
        fixture
            .store
            .finalize_mutation(
                attempt.attempt_id,
                fixture.repo.path(),
                "src/file.rs".to_string(),
            )
            .await,
        Err(StoreError::MutationAlreadyFinalized { .. })
    ));
}

#[tokio::test]
async fn mutation_evidence_records_workspace_epochs() {
    let fixture = Fixture::new().await;
    tokio::fs::create_dir_all(fixture.repo.path().join("src"))
        .await
        .expect("source directory");
    tokio::fs::write(fixture.repo.path().join("src/file.rs"), b"before")
        .await
        .expect("prewrite file");
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("epoch-root", "src"))
        .await
        .expect("worker assignment");
    let start_revision = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["src/file.rs".to_string()])
        .await
        .expect("start revision captures");
    fixture
        .store
        .begin_mutation(
            attempt.attempt_id,
            fixture.repo.path(),
            "src/file.rs".to_string(),
            AttributionConfidence::Definitive,
        )
        .await
        .expect("mutation begins");
    tokio::fs::write(fixture.repo.path().join("src/file.rs"), b"after")
        .await
        .expect("mutated file");
    let end_revision = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["src/file.rs".to_string()])
        .await
        .expect("end revision captures");
    let evidence = fixture
        .store
        .finalize_mutation(
            attempt.attempt_id,
            fixture.repo.path(),
            "src/file.rs".to_string(),
        )
        .await
        .expect("mutation finalizes");
    assert_eq!(evidence.start_epoch, start_revision.epoch);
    assert_eq!(evidence.end_epoch, Some(end_revision.epoch));
    assert!(end_revision.epoch > start_revision.epoch);

    fixture.store.close().await;
    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("task store restarts");
    let persisted = restarted
        .list_mutation_evidence(attempt.attempt_id, None)
        .await
        .expect("persisted mutation evidence reads");
    assert_eq!(persisted, vec![evidence]);
}

async fn finalized_snapshot_assignment(
    fixture: &Fixture,
    root_session_id: &str,
) -> (Assignment, Attempt) {
    tokio::fs::create_dir_all(fixture.repo.path().join("src"))
        .await
        .expect("source directory");
    tokio::fs::write(fixture.repo.path().join("src/file.rs"), b"before")
        .await
        .expect("prewrite file");
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft(root_session_id, "src"))
        .await
        .expect("worker assignment");
    fixture
        .store
        .begin_mutation(
            attempt.attempt_id,
            fixture.repo.path(),
            "src/file.rs".to_string(),
            AttributionConfidence::Definitive,
        )
        .await
        .expect("mutation begins");
    tokio::fs::write(fixture.repo.path().join("src/file.rs"), b"after")
        .await
        .expect("mutated file");
    fixture
        .store
        .finalize_mutation(
            attempt.attempt_id,
            fixture.repo.path(),
            "src/file.rs".to_string(),
        )
        .await
        .expect("mutation finalizes");
    (assignment, attempt)
}

async fn snapshot_is_retained(store: &LocalAgentTaskStore, attempt_id: AttemptId) -> bool {
    store
        .list_mutation_evidence(attempt_id, None)
        .await
        .expect("mutation evidence reads")
        .into_iter()
        .next()
        .expect("mutation evidence exists")
        .snapshot_retained
}

#[tokio::test]
async fn final_receipt_collects_snapshots() {
    let fixture = Fixture::new().await;
    let (_, attempt) = finalized_snapshot_assignment(&fixture, "gc-receipt-root").await;

    assert!(snapshot_is_retained(&fixture.store, attempt.attempt_id).await);
    fixture
        .store
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt_with_changes(Vec::new(), &["src/file.rs"]),
        )
        .await
        .expect("final receipt seals and collects snapshots");

    assert!(!snapshot_is_retained(&fixture.store, attempt.attempt_id).await);
    assert!(matches!(
        fixture
            .store
            .read_mutation_snapshot(
                attempt.attempt_id,
                "src/file.rs".to_string(),
                MutationSnapshotVersion::PreWrite,
                0,
                None,
            )
            .await,
        Err(StoreError::SnapshotUnavailable { .. })
    ));
}

#[tokio::test]
async fn abandonment_collects_snapshots() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = finalized_snapshot_assignment(&fixture, "gc-abandon-root").await;

    fixture
        .store
        .abandon_agent_task(
            TaskActor::Root,
            assignment.assignment_id,
            "root abandons the task".to_string(),
        )
        .await
        .expect("abandonment seals the task and collects snapshots");

    assert!(!snapshot_is_retained(&fixture.store, attempt.attempt_id).await);
}

#[tokio::test]
async fn pending_gate_retains_snapshots_until_sealed_or_waived() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = finalized_snapshot_assignment(&fixture, "gc-gate-root").await;
    fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            assignment.assignment_id,
            GateKind::Verification,
            GateStatus::Pending,
            "verification pending".to_string(),
        )
        .await
        .expect("pending verification gate");
    fixture
        .store
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt_with_changes(Vec::new(), &["src/file.rs"]),
        )
        .await
        .expect("receipt seals while gate remains pending");
    assert!(snapshot_is_retained(&fixture.store, attempt.attempt_id).await);

    fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            assignment.assignment_id,
            GateKind::Verification,
            GateStatus::Passed,
            "verification passed".to_string(),
        )
        .await
        .expect("last gate seals");
    assert!(!snapshot_is_retained(&fixture.store, attempt.attempt_id).await);

    let waived_fixture = Fixture::new().await;
    let (waived_assignment, waived_attempt) =
        finalized_snapshot_assignment(&waived_fixture, "gc-waiver-root").await;
    waived_fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            waived_assignment.assignment_id,
            GateKind::Verification,
            GateStatus::Pending,
            "verification pending".to_string(),
        )
        .await
        .expect("pending verification gate");
    waived_fixture
        .store
        .submit_agent_receipt(
            waived_attempt.attempt_id,
            completed_receipt_with_changes(Vec::new(), &["src/file.rs"]),
        )
        .await
        .expect("receipt seals while gate remains pending");
    waived_fixture
        .store
        .waive_agent_gate(
            TaskActor::Root,
            waived_assignment.assignment_id,
            GateKind::Verification,
            "root accepts verification risk".to_string(),
        )
        .await
        .expect("last gate is waived");
    assert!(!snapshot_is_retained(&waived_fixture.store, waived_attempt.attempt_id).await);
}

#[tokio::test]
async fn risk_review_creates_verification_gate_before_snapshot_collection() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = finalized_snapshot_assignment(&fixture, "gc-review-root").await;
    fixture
        .store
        .submit_agent_receipt_with_review(
            attempt.attempt_id,
            completed_receipt_with_changes(Vec::new(), &["src/file.rs"]),
            "cold review required: focused validation unavailable".to_string(),
        )
        .await
        .expect("review-gated receipt seals");
    fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            assignment.assignment_id,
            GateKind::Review,
            GateStatus::Passed,
            "review passed".to_string(),
        )
        .await
        .expect("review passes and creates verification gate");
    assert!(snapshot_is_retained(&fixture.store, attempt.attempt_id).await);

    fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            assignment.assignment_id,
            GateKind::Verification,
            GateStatus::Passed,
            "verification passed".to_string(),
        )
        .await
        .expect("verification passes");
    assert!(!snapshot_is_retained(&fixture.store, attempt.attempt_id).await);
}

#[tokio::test]
async fn correction_attempt_retains_snapshots_until_every_attempt_and_gate_is_sealed() {
    let fixture = Fixture::new().await;
    let (assignment, initial_attempt) =
        finalized_snapshot_assignment(&fixture, "gc-correction-root").await;
    fixture
        .store
        .submit_agent_receipt_with_review(
            initial_attempt.attempt_id,
            completed_receipt_with_changes(Vec::new(), &["src/file.rs"]),
            "cold review required: correction review".to_string(),
        )
        .await
        .expect("initial receipt seals behind review");
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
        .expect("review requests correction");
    assert!(snapshot_is_retained(&fixture.store, initial_attempt.attempt_id).await);

    let correction = fixture
        .store
        .amend_agent_task(
            TaskActor::Root,
            assignment.assignment_id,
            AttemptAmendment {
                reason: "address the review finding".to_string(),
                objective: None,
                acceptance_criteria: None,
                stop_condition: None,
            },
        )
        .await
        .expect("correction attempt starts");
    assert!(snapshot_is_retained(&fixture.store, initial_attempt.attempt_id).await);
    fixture
        .store
        .submit_agent_receipt(correction.attempt_id, completed_receipt(Vec::new()))
        .await
        .expect("correction receipt seals");
    assert!(snapshot_is_retained(&fixture.store, initial_attempt.attempt_id).await);
    fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            assignment.assignment_id,
            GateKind::Review,
            GateStatus::ChangesRequested,
            "the bounded correction remains unresolved".to_string(),
        )
        .await
        .expect("the final correction verdict seals without reopening work");
    assert!(!snapshot_is_retained(&fixture.store, initial_attempt.attempt_id).await);
}

#[tokio::test]
async fn startup_reuses_live_eligibility_and_never_collects_for_terminal_status_alone() {
    let eligible = Fixture::new().await;
    let (eligible_assignment, eligible_attempt) =
        finalized_snapshot_assignment(&eligible, "gc-startup-eligible").await;
    eligible
        .store
        .set_agent_gate(
            TaskActor::Root,
            eligible_assignment.assignment_id,
            GateKind::Verification,
            GateStatus::Pending,
            "verification pending".to_string(),
        )
        .await
        .expect("pending gate");
    eligible
        .store
        .submit_agent_receipt(
            eligible_attempt.attempt_id,
            completed_receipt_with_changes(Vec::new(), &["src/file.rs"]),
        )
        .await
        .expect("receipt seals with retained snapshots");
    eligible.store.close().await;
    let pool = coordination_pool(&eligible).await;
    let now = Utc::now();
    let mut gate: AgentGate = serde_json::from_str(
        &sqlx::query_scalar::<_, String>(
            "SELECT body_json FROM gates WHERE assignment_id = ? AND kind = ?",
        )
        .bind(eligible_assignment.assignment_id.to_string())
        .bind(serde_json::to_string(&GateKind::Verification).expect("gate kind serializes"))
        .fetch_one(&pool)
        .await
        .expect("pending gate reads"),
    )
    .expect("pending gate decodes");
    gate.status = GateStatus::Passed;
    gate.updated_at = now;
    gate.sealed_at = Some(now);
    sqlx::query(
        "UPDATE gates SET status = ?, body_json = ?, updated_at = ?, sealed_at = ?
         WHERE assignment_id = ? AND kind = ?",
    )
    .bind(serde_json::to_string(&gate.status).expect("gate status serializes"))
    .bind(serde_json::to_string(&gate).expect("gate serializes"))
    .bind(serde_json::to_string(&now).expect("time serializes"))
    .bind(serde_json::to_string(&now).expect("time serializes"))
    .bind(eligible_assignment.assignment_id.to_string())
    .bind(serde_json::to_string(&GateKind::Verification).expect("gate kind serializes"))
    .execute(&pool)
    .await
    .expect("legacy retained assignment becomes eligible");
    pool.close().await;
    let restarted = LocalAgentTaskStore::initialize(&eligible.state)
        .await
        .expect("eligible store restarts");
    assert!(!snapshot_is_retained(&restarted, eligible_attempt.attempt_id).await);

    let incomplete = Fixture::new().await;
    let (_, incomplete_attempt) =
        finalized_snapshot_assignment(&incomplete, "gc-startup-incomplete").await;
    incomplete.store.close().await;
    let pool = coordination_pool(&incomplete).await;
    let sealed_at = Utc::now();
    sqlx::query("UPDATE attempts SET state = ?, sealed_at = ? WHERE attempt_id = ?")
        .bind(serde_json::to_string(&AttemptState::Abandoned).expect("state serializes"))
        .bind(serde_json::to_string(&sealed_at).expect("time serializes"))
        .bind(incomplete_attempt.attempt_id.to_string())
        .execute(&pool)
        .await
        .expect("attempt is made terminal without a receipt");
    pool.close().await;
    let restarted = LocalAgentTaskStore::initialize(&incomplete.state)
        .await
        .expect("incomplete store restarts");
    assert!(snapshot_is_retained(&restarted, incomplete_attempt.attempt_id).await);

    let pending = Fixture::new().await;
    let (pending_assignment, pending_attempt) =
        finalized_snapshot_assignment(&pending, "gc-startup-pending").await;
    pending
        .store
        .set_agent_gate(
            TaskActor::Root,
            pending_assignment.assignment_id,
            GateKind::Verification,
            GateStatus::Pending,
            "verification remains pending".to_string(),
        )
        .await
        .expect("pending gate");
    pending
        .store
        .submit_agent_receipt(
            pending_attempt.attempt_id,
            completed_receipt_with_changes(Vec::new(), &["src/file.rs"]),
        )
        .await
        .expect("receipt seals behind pending gate");
    pending.store.close().await;
    let restarted = LocalAgentTaskStore::initialize(&pending.state)
        .await
        .expect("pending-gate store restarts");
    assert!(snapshot_is_retained(&restarted, pending_attempt.attempt_id).await);

    let correction = Fixture::new().await;
    let (correction_assignment, correction_initial_attempt) =
        finalized_snapshot_assignment(&correction, "gc-startup-correction").await;
    correction
        .store
        .submit_agent_receipt_with_review(
            correction_initial_attempt.attempt_id,
            completed_receipt_with_changes(Vec::new(), &["src/file.rs"]),
            "cold review required: startup correction".to_string(),
        )
        .await
        .expect("review-gated receipt seals");
    correction
        .store
        .set_agent_gate(
            TaskActor::Root,
            correction_assignment.assignment_id,
            GateKind::Review,
            GateStatus::ChangesRequested,
            "correction required".to_string(),
        )
        .await
        .expect("review requests correction");
    correction.store.close().await;
    let restarted = LocalAgentTaskStore::initialize(&correction.state)
        .await
        .expect("changes-requested store restarts");
    assert!(
        snapshot_is_retained(&restarted, correction_initial_attempt.attempt_id).await,
        "the original changes-requested attempt can still reopen work"
    );
    restarted
        .amend_agent_task(
            TaskActor::Root,
            correction_assignment.assignment_id,
            AttemptAmendment {
                reason: "complete the correction".to_string(),
                objective: None,
                acceptance_criteria: None,
                stop_condition: None,
            },
        )
        .await
        .expect("active correction attempt starts");
    restarted.close().await;
    let restarted = LocalAgentTaskStore::initialize(&correction.state)
        .await
        .expect("correction store restarts");
    assert!(
        snapshot_is_retained(&restarted, correction_initial_attempt.attempt_id).await,
        "an active correction attempt with no receipt remains ineligible"
    );
}

#[tokio::test]
async fn failed_snapshot_deletion_is_queued_and_retried_without_failing_receipt() {
    let fixture = Fixture::new().await;
    let (_, attempt) = finalized_snapshot_assignment(&fixture, "gc-retry-root").await;
    let pool = coordination_pool(&fixture).await;
    let snapshot_name = sqlx::query_scalar::<_, String>(
        "SELECT snapshot_name FROM mutation_files WHERE attempt_id = ? AND path = ?",
    )
    .bind(attempt.attempt_id.to_string())
    .bind("src/file.rs")
    .fetch_one(&pool)
    .await
    .expect("snapshot name reads");
    pool.close().await;
    let snapshot_path = fixture
        .state
        .codex_home()
        .join("agent-task-coordination")
        .join(snapshot_name);
    tokio::fs::remove_file(&snapshot_path)
        .await
        .expect("snapshot file is replaced for failure injection");
    tokio::fs::create_dir(&snapshot_path)
        .await
        .expect("directory forces remove_file failure");

    fixture
        .store
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt_with_changes(Vec::new(), &["src/file.rs"]),
        )
        .await
        .expect("receipt remains successful when deletion fails");
    assert!(!snapshot_is_retained(&fixture.store, attempt.attempt_id).await);
    let pool = coordination_pool(&fixture).await;
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM snapshot_gc_queue")
            .fetch_one(&pool)
            .await
            .expect("queued deletion count reads")
            > 0
    );
    pool.close().await;

    tokio::fs::remove_dir(&snapshot_path)
        .await
        .expect("failure injection is removed");
    fixture.store.close().await;
    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("queued deletion retries at startup");
    let pool = coordination_pool(&fixture).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM snapshot_gc_queue")
            .fetch_one(&pool)
            .await
            .expect("queue drains after retry"),
        0
    );
    pool.close().await;
    restarted.close().await;
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
    assert_eq!(decision.reasons.len(), 4);
    assert!(GateKind::Review.is_waivable());
    assert!(GateKind::Verification.is_waivable());
    assert!(!GateKind::Mutation.is_waivable());
    assert!(!GateKind::Ownership.is_waivable());
}

#[test]
fn risk_gate_uses_canonical_concurrent_drift_reason() {
    let decision = evaluate_risk_gate(&RiskFacts {
        focused_validation_succeeded: true,
        drift: true,
        ..RiskFacts::default()
    });

    assert_eq!(decision.reasons, vec![CONCURRENT_DRIFT_REASON.to_string()]);
}

#[tokio::test]
async fn repository_wide_capture_detects_an_external_revert_missing_from_git_overlay() {
    let fixture = Fixture::new().await;
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(fixture.repo.path())
            .args(args)
            .output()
            .expect("git command launches")
    };
    assert!(git(&["init", "-q"]).status.success());
    assert!(
        git(&["config", "user.email", "coordination@example.invalid"])
            .status
            .success()
    );
    assert!(
        git(&["config", "user.name", "Coordination Test"])
            .status
            .success()
    );
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "base\n").expect("base source");
    assert!(git(&["add", "src/lib.rs"]).status.success());
    assert!(git(&["commit", "-qm", "base"]).status.success());

    std::fs::write(fixture.repo.path().join("src/lib.rs"), "modified\n")
        .expect("external modification");
    let modified = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec![REPOSITORY_WIDE_PATH.to_string()])
        .await
        .expect("modified overlay is captured");
    assert_eq!(
        modified
            .files
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec![REPOSITORY_WIDE_PATH, "src/lib.rs"]
    );

    std::fs::write(fixture.repo.path().join("src/lib.rs"), "base\n").expect("external revert");
    let reverted = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec![REPOSITORY_WIDE_PATH.to_string()])
        .await
        .expect("reverted overlay is reconciled");
    assert!(reverted.epoch > modified.epoch);
    assert_eq!(
        reverted
            .files
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec![REPOSITORY_WIDE_PATH, "src/lib.rs"]
    );
    let events = fixture
        .store
        .read_workspace_events(fixture.repo.path(), modified.epoch)
        .await
        .expect("revert drift event reads");
    assert!(events.iter().any(|event| {
        event.actor_kind == WorkspaceActorKind::External
            && event.attribution_confidence == AttributionConfidence::DetectionOnly
            && event.paths == vec!["src/lib.rs".to_string()]
    }));
}

#[tokio::test]
async fn repository_wide_capture_detects_clean_head_change() {
    let fixture = Fixture::new().await;
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(fixture.repo.path())
            .args(args)
            .output()
            .expect("git command launches")
    };
    assert!(git(&["init", "-q"]).status.success());
    assert!(
        git(&["config", "user.email", "coordination@example.invalid"])
            .status
            .success()
    );
    assert!(
        git(&["config", "user.name", "Coordination Test"])
            .status
            .success()
    );
    std::fs::write(fixture.repo.path().join("tracked.txt"), "first\n").expect("first revision");
    assert!(git(&["add", "tracked.txt"]).status.success());
    assert!(git(&["commit", "-qm", "first"]).status.success());

    let first = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec![REPOSITORY_WIDE_PATH.to_string()])
        .await
        .expect("first head is captured");
    std::fs::write(fixture.repo.path().join("tracked.txt"), "second\n").expect("second revision");
    assert!(git(&["add", "tracked.txt"]).status.success());
    assert!(git(&["commit", "-qm", "second"]).status.success());

    let second = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec![REPOSITORY_WIDE_PATH.to_string()])
        .await
        .expect("second head is captured");
    assert!(second.epoch > first.epoch);
    assert_ne!(first.manifest_hash, second.manifest_hash);
    let events = fixture
        .store
        .read_workspace_events(fixture.repo.path(), first.epoch)
        .await
        .expect("head drift event reads");
    assert!(events.iter().any(|event| {
        event.actor_kind == WorkspaceActorKind::External
            && event.attribution_confidence == AttributionConfidence::DetectionOnly
            && event.paths == vec![REPOSITORY_WIDE_PATH.to_string()]
    }));
}

#[tokio::test]
async fn unrelated_work_in_the_same_repository_warns_without_blocking_quiescence() {
    let fixture = Fixture::new().await;
    let (completed_assignment, _) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("completed-root", "completed"),
        )
        .await
        .expect("completed-root assignment");
    fixture
        .store
        .abandon_agent_task(
            TaskActor::Root,
            completed_assignment.assignment_id,
            "root approved terminal cleanup".to_string(),
        )
        .await
        .expect("completed-root assignment becomes terminal");
    let (unrelated_assignment, _) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("unrelated-root", "unrelated"),
        )
        .await
        .expect("unrelated assignment");

    let status = fixture
        .store
        .check_quiescence("completed-root".to_string())
        .await
        .expect("completed root quiescence");

    assert!(
        status.quiescent,
        "unrelated task roots must not block completion"
    );
    assert!(
        status.warnings.iter().any(|warning| {
            warning.contains("unrelated-root")
                && warning.contains(&unrelated_assignment.assignment_id.to_string())
        }),
        "active work in the same repository lineage is surfaced as a warning: {:?}",
        status.warnings
    );
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
                before_deadline - Duration::seconds(120),
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
                before_deadline - Duration::seconds(120),
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
            recovery_threshold_seconds: 120,
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
async fn quiescence_reports_claim_metadata_without_waiting_on_it() {
    let fixture = Fixture::new().await;
    let (assignment, _) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("metadata-quiescence-root", "src/lib.rs"),
        )
        .await
        .expect("metadata assignment is admitted");
    fixture
        .store
        .abandon_agent_task(
            TaskActor::Root,
            assignment.assignment_id,
            "terminal fixture".to_string(),
        )
        .await
        .expect("assignment becomes terminal");

    let pool = coordination_pool(&fixture).await;
    sqlx::query(
        "UPDATE write_claims
         SET active = 1, released_at = NULL
         WHERE assignment_id = ?",
    )
    .bind(assignment.assignment_id.to_string())
    .execute(&pool)
    .await
    .expect("active claim metadata is restored");
    pool.close().await;
    let status = fixture
        .store
        .inspect_quiescence("metadata-quiescence-root".to_string())
        .await
        .expect("quiescence inspection reads");
    assert!(status.quiescent);
    assert_eq!(
        status.active_claim_assignment_ids,
        vec![assignment.assignment_id]
    );
}

#[tokio::test]
async fn nudge_leases_quiescence_and_restart_are_durable() {
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
    let quiescence = fixture
        .store
        .check_quiescence("restart-root".to_string())
        .await
        .expect("active quiescence reads");
    assert!(!quiescence.quiescent);
    assert_eq!(
        quiescence.active_assignment_ids,
        vec![assignment.assignment_id]
    );

    fixture.store.close().await;
    std::fs::write(
        fixture.repo.path().join("src/lib.rs"),
        "changed during restart\n",
    )
    .expect("restart drift");
    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("store reconstructs");
    let revision = restarted
        .capture_workspace_revision(fixture.repo.path(), vec!["src/lib.rs".to_string()])
        .await
        .expect("restart detects drift");
    assert!(revision.epoch > call.evidence.start_epoch);
    let restarted_quiescence = restarted
        .check_quiescence("restart-root".to_string())
        .await
        .expect("restarted quiescence reads");
    assert!(
        restarted_quiescence
            .active_claim_assignment_ids
            .contains(&assignment.assignment_id),
        "claim metadata reconstructs across restart"
    );
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
    let stale_error = restarted
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt(vec!["restart-validation".to_string()]),
        )
        .await
        .expect_err("workspace drift supersedes the independently recorded validation");
    assert!(matches!(
        stale_error,
        StoreError::EvidenceSuperseded { call_ids }
            if call_ids == vec!["restart-validation".to_string()]
    ));
    let stale_quiescence = restarted
        .check_quiescence("restart-root".to_string())
        .await
        .expect("stale quiescence reads");
    assert!(
        !stale_quiescence.quiescent,
        "the active assignment must remain visible after stale validation is rejected"
    );
    assert_eq!(
        stale_quiescence.active_assignment_ids,
        vec![assignment.assignment_id]
    );
    assert!(stale_quiescence.running_validation_call_ids.is_empty());
    assert!(stale_quiescence.pending_gate_assignment_ids.is_empty());
    assert_eq!(
        stale_quiescence.active_claim_assignment_ids,
        vec![assignment.assignment_id]
    );
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
    restarted
        .begin_mutation(
            attempt.attempt_id,
            fixture.repo.path(),
            "src/lib.rs".to_string(),
            AttributionConfidence::Definitive,
        )
        .await
        .expect("upgraded assignment may record mutation evidence in its bound workspace");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "after\n").expect("upgraded mutation");
    restarted
        .finalize_mutation(
            attempt.attempt_id,
            fixture.repo.path(),
            "src/lib.rs".to_string(),
        )
        .await
        .expect("upgraded mutation finalizes");
}

#[tokio::test]
async fn isolated_overlap_integrates_only_through_versioned_handoff() {
    let fixture = Fixture::new().await;
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(fixture.repo.path())
            .args(args)
            .output()
            .expect("git command launches")
    };
    assert!(git(&["init", "-q"]).status.success());
    assert!(
        git(&["config", "user.email", "coordination@example.invalid"])
            .status
            .success()
    );
    assert!(
        git(&["config", "user.name", "Coordination Test"])
            .status
            .success()
    );
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "base\n").expect("base source");
    assert!(git(&["add", "src/lib.rs"]).status.success());
    assert!(git(&["commit", "-qm", "base"]).status.success());
    let isolated_path = fixture
        .repo
        .path()
        .parent()
        .expect("repo parent")
        .join(format!("isolated-{}", Uuid::now_v7()));
    let worktree = Command::new("git")
        .arg("-C")
        .arg(fixture.repo.path())
        .args(["worktree", "add", "--detach"])
        .arg(&isolated_path)
        .arg("HEAD")
        .output()
        .expect("worktree add launches");
    assert!(
        worktree.status.success(),
        "{}",
        String::from_utf8_lossy(&worktree.stderr)
    );

    let (shared, shared_attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("isolation-root", "src/lib.rs"),
        )
        .await
        .expect("shared implementation claims the path");
    let isolated_command = "cargo test -p owner isolated";
    let mut isolated_draft =
        validation_worker_draft("isolation-root", "src/lib.rs", isolated_command);
    isolated_draft.workspace_strategy = WorkspaceStrategy::Isolated;
    let (isolated, isolated_attempt) = fixture
        .store
        .create_assignment(&isolated_path, isolated_draft)
        .await
        .expect("intentional overlap uses a separate workspace");
    assert_eq!(shared.repository_id, isolated.repository_id);
    assert_ne!(shared.workspace_id, isolated.workspace_id);
    controlled_write(
        &fixture.store,
        &isolated_path,
        "isolation-root",
        isolated.assignment_id,
        isolated_attempt.attempt_id,
        "src/lib.rs",
        "isolated implementation\n",
    )
    .await;
    finish_focused_validation(
        &fixture.store,
        start_focused_validation(
            &fixture.store,
            isolated_attempt.attempt_id,
            "isolated-validation",
            isolated_command,
        )
        .await,
    )
    .await;
    fixture
        .store
        .submit_agent_receipt(
            isolated_attempt.attempt_id,
            completed_receipt_with_changes(
                vec!["isolated-validation".to_string()],
                &["src/lib.rs"],
            ),
        )
        .await
        .expect("isolated receipt publishes handoff");
    let ready = fixture
        .store
        .get_agent_task(isolated.assignment_id, Some(0))
        .await
        .expect("isolated handoff reads")
        .isolation_handoff
        .expect("isolated handoff exists");
    assert_eq!(ready.state, IsolationHandoffState::Ready);
    let isolated_canonical = std::fs::canonicalize(&isolated_path)
        .expect("isolated path canonicalizes")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        ready.source_repository_root.as_deref(),
        Some(isolated_canonical.as_str()),
        "the integrator handoff exposes the durable source workspace for inspection"
    );

    let mut shared_receipt = completed_receipt(Vec::new());
    shared_receipt.status = AgentStatusClaim::NeedsMain;
    shared_receipt.summary = "shared implementation yielded to integrator".to_string();
    shared_receipt.criterion_results[0].status = CriterionStatus::NotRun;
    fixture
        .store
        .submit_agent_receipt(shared_attempt.attempt_id, shared_receipt)
        .await
        .expect("shared claim releases");

    let integrator_command = "cargo test -p owner integrated";
    let integrator_draft = AssignmentDraft {
        root_session_id: "isolation-root".to_string(),
        admission_origin: AssignmentAdmissionOrigin::Typed,
        role: AgentRole::Integrator,
        capability_profile: CapabilityProfile::IntegratorSourceWrite,
        objective: "integrate the versioned isolated result".to_string(),
        acceptance_criteria: vec![criterion()],
        read_scope: Vec::new(),
        write_scope: vec![RepoScope {
            path: "src/lib.rs".to_string(),
            recursive: true,
        }],
        stop_condition: "stop after versioned integration".to_string(),
        dependencies: vec![isolated.assignment_id],
        risk_hints: Vec::new(),
        required_evidence: vec![integrator_command.to_string()],
        prohibited_changes: Vec::new(),
        contract_claims: Vec::new(),
        workspace_strategy: WorkspaceStrategy::Shared,
        relation: Some(AssignmentRelation {
            kind: RelationKind::Integration,
            target_assignment_ids: vec![isolated.assignment_id],
        }),
        architecture_contract_ref: None,
    };
    let (integrator, _integrator_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), integrator_draft.clone())
        .await
        .expect("integrator claims ready handoff");
    let claimed = fixture
        .store
        .get_agent_task(isolated.assignment_id, Some(0))
        .await
        .expect("claimed handoff reads")
        .isolation_handoff
        .expect("claimed handoff exists");
    assert_eq!(claimed.state, IsolationHandoffState::Claimed);
    assert_eq!(
        claimed.integrator_assignment_id,
        Some(integrator.assignment_id)
    );
    fixture
        .store
        .abandon_agent_task(
            TaskActor::Root,
            integrator.assignment_id,
            "integrator stopped before applying the handoff".to_string(),
        )
        .await
        .expect("abandoned integrator releases its handoff claim");
    let released = fixture
        .store
        .get_agent_task(isolated.assignment_id, Some(0))
        .await
        .expect("released handoff reads")
        .isolation_handoff
        .expect("released handoff exists");
    assert_eq!(released.state, IsolationHandoffState::Ready);
    assert_eq!(released.integrator_assignment_id, None);

    let (replacement_integrator, replacement_integrator_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), integrator_draft)
        .await
        .expect("replacement integrator reclaims released handoff");
    controlled_write(
        &fixture.store,
        fixture.repo.path(),
        "isolation-root",
        replacement_integrator.assignment_id,
        replacement_integrator_attempt.attempt_id,
        "src/lib.rs",
        "isolated implementation\n",
    )
    .await;
    finish_focused_validation(
        &fixture.store,
        start_focused_validation(
            &fixture.store,
            replacement_integrator_attempt.attempt_id,
            "integrator-validation",
            integrator_command,
        )
        .await,
    )
    .await;
    fixture
        .store
        .submit_agent_receipt(
            replacement_integrator_attempt.attempt_id,
            completed_receipt_with_changes(
                vec!["integrator-validation".to_string()],
                &["src/lib.rs"],
            ),
        )
        .await
        .expect("integrator seals versioned handoff");
    assert_eq!(
        fixture
            .store
            .get_agent_task(isolated.assignment_id, Some(0))
            .await
            .expect("integrated handoff reads")
            .isolation_handoff
            .expect("integrated handoff exists")
            .state,
        IsolationHandoffState::Integrated
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo.path().join("src/lib.rs"))
            .expect("integrated file reads"),
        "isolated implementation\n"
    );

    let cleanup = Command::new("git")
        .arg("-C")
        .arg(fixture.repo.path())
        .args(["worktree", "remove", "--force"])
        .arg(&isolated_path)
        .output()
        .expect("worktree cleanup launches");
    assert!(cleanup.status.success());
}
