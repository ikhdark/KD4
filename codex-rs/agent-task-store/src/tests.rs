use chrono::Duration;
use chrono::Utc;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

use super::*;

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

async fn validation_evidence_revision(pool: &sqlx::SqlitePool, attempt_id: AttemptId) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT revision FROM validation_evidence_revisions WHERE attempt_id = ?",
    )
    .bind(attempt_id.to_string())
    .fetch_optional(pool)
    .await
    .expect("revision reads")
    .unwrap_or(0)
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
async fn workspace_mutation_admission_waits_for_transient_writer_contention() {
    let fixture = Fixture::new().await;
    let blocker_pool = coordination_pool(&fixture).await;
    let mut blocker = blocker_pool
        .acquire()
        .await
        .expect("coordination connection opens");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *blocker)
        .await
        .expect("writer lock is acquired");

    let mutation = fixture.store.begin_workspace_mutation(
        fixture.repo.path(),
        WorkspaceMutationRequest {
            root_session_id: "contended-mutation-root".to_string(),
            actor_id: "root:contended-mutation-root".to_string(),
            kind: WorkspaceActorKind::Root,
            attempt_id: None,
            paths: vec![REPOSITORY_WIDE_PATH.to_string()],
            contracts: Vec::new(),
            expected_manifest: Vec::new(),
        },
    );
    tokio::pin!(mutation);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut mutation)
            .await
            .is_err(),
        "mutation admission should wait while another connection owns the writer lock"
    );

    sqlx::query("ROLLBACK")
        .execute(&mut *blocker)
        .await
        .expect("writer lock is released");
    let lease = tokio::time::timeout(std::time::Duration::from_secs(1), mutation)
        .await
        .expect("mutation admission resumes promptly after the writer lock is released")
        .expect("mutation admission survives transient writer contention");
    drop(blocker);
    blocker_pool.close().await;

    fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), lease)
        .await
        .expect("contended mutation lease releases");
}

#[tokio::test]
async fn repository_wide_manifests_are_referenced_reused_and_restart_decodable() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.repo.path().join("overlay.txt"), "before\n").expect("overlay fixture");
    let request = WorkspaceMutationRequest {
        root_session_id: "manifest-reference-root".to_string(),
        actor_id: "root:manifest-reference-root".to_string(),
        kind: WorkspaceActorKind::Root,
        attempt_id: None,
        paths: vec![REPOSITORY_WIDE_PATH.to_string()],
        contracts: Vec::new(),
        expected_manifest: Vec::new(),
    };
    let lease = fixture
        .store
        .begin_workspace_mutation(fixture.repo.path(), request.clone())
        .await
        .expect("repository-wide lease starts");
    let pool = coordination_pool(&fixture).await;
    let stored = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT expected_manifest_json, expected_manifest_storage_kind,
                expected_manifest_reference_hash
         FROM workspace_mutation_leases WHERE lease_id = ?",
    )
    .bind(&lease.lease_id)
    .fetch_one(&pool)
    .await
    .expect("stored manifest reference reads");
    assert_eq!(stored.1, "content_addressed_v1");
    let reference_hash = stored.2.as_deref().expect("reference hash");
    let reference: serde_json::Value =
        serde_json::from_str(&stored.0).expect("tagged manifest reference");
    assert_eq!(reference["storage"], "content_addressed");
    assert_eq!(reference["tag_version"], 1);
    assert_eq!(reference["payload_format_version"], 1);
    assert_eq!(reference["manifest_id"], reference_hash);
    assert!(
        serde_json::from_str::<Vec<WorkspaceManifestEntry>>(&stored.0).is_err(),
        "a legacy array reader must fail instead of treating a reference as empty"
    );
    let referenced_entries = crate::manifest_storage::decode_manifest(
        &pool,
        &lease.workspace_id,
        &stored.0,
        &stored.1,
        stored.2.as_deref(),
    )
    .await
    .expect("referenced manifest reads before disabling new reference writes");
    let mut disabled_write = pool.begin().await.expect("write-policy transaction starts");
    let inline = crate::manifest_storage::encode_manifest(
        &mut disabled_write,
        &lease.workspace_id,
        &referenced_entries,
        false,
    )
    .await
    .expect("disabled reference writes fall back to inline storage");
    assert_eq!(inline.storage_kind, crate::manifest_storage::INLINE_V1);
    assert!(inline.reference_hash.is_none());
    disabled_write
        .rollback()
        .await
        .expect("write-policy transaction rolls back");
    assert_eq!(
        crate::manifest_storage::decode_manifest(
            &pool,
            &lease.workspace_id,
            &stored.0,
            &stored.1,
            stored.2.as_deref(),
        )
        .await
        .expect("existing reference remains readable while new reference writes are disabled"),
        referenced_entries
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM workspace_manifest_payloads")
            .fetch_one(&pool)
            .await
            .expect("payload count"),
        1
    );
    pool.close().await;

    fixture.store.close().await;
    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("store restarts with referenced lease");
    restarted
        .finish_workspace_mutation(fixture.repo.path(), lease)
        .await
        .expect("referenced manifest decodes after restart");
    let second = restarted
        .begin_workspace_mutation(fixture.repo.path(), request)
        .await
        .expect("second repository-wide lease starts");
    let second_workspace_id = second.workspace_id.clone();
    let pool = coordination_pool(&fixture).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM workspace_manifest_payloads")
            .fetch_one(&pool)
            .await
            .expect("deduplicated payload count"),
        1,
        "an unchanged canonical manifest reuses its payload"
    );
    pool.close().await;
    restarted
        .finish_workspace_mutation(fixture.repo.path(), second)
        .await
        .expect("second lease releases");
    let pool = coordination_pool(&fixture).await;
    let head = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT head_manifest_json, head_manifest_storage_kind,
                head_manifest_reference_hash
         FROM workspace_repositories WHERE workspace_id = ?",
    )
    .bind(&second_workspace_id)
    .fetch_one(&pool)
    .await
    .expect("workspace head reads");
    assert_eq!(head.1, "content_addressed_v1");
    let head_hash = head.2.as_deref().expect("head reference hash");
    let head_reference: serde_json::Value =
        serde_json::from_str(&head.0).expect("tagged head manifest reference");
    assert_eq!(head_reference["storage"], "content_addressed");
    assert_eq!(head_reference["tag_version"], 1);
    assert_eq!(head_reference["payload_format_version"], 1);
    assert_eq!(head_reference["manifest_id"], head_hash);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM workspace_manifest_payloads")
            .fetch_one(&pool)
            .await
            .expect("retained payload count"),
        1,
        "lease release and final-head publication retain the unique payload"
    );
    let expected_head = head.clone();
    pool.close().await;

    let exact = restarted
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "exact-after-repository-root".to_string(),
                actor_id: "root:exact-after-repository-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec!["overlay.txt".to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("exact-path lease starts after repository head publication");
    restarted
        .finish_workspace_mutation(fixture.repo.path(), exact)
        .await
        .expect("exact-path lease releases");
    let pool = coordination_pool(&fixture).await;
    let head_after_exact = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT head_manifest_json, head_manifest_storage_kind,
                head_manifest_reference_hash
         FROM workspace_repositories WHERE workspace_id = ?",
    )
    .bind(&second_workspace_id)
    .fetch_one(&pool)
    .await
    .expect("workspace head reads after exact-path finalization");
    assert_eq!(
        head_after_exact, expected_head,
        "an exact-path final manifest must not replace the repository-wide head"
    );
    pool.close().await;
}

#[tokio::test]
async fn referenced_empty_legacy_inline_and_unknown_storage_remain_distinct() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.repo.path().join("legacy.txt"), "legacy\n")
        .expect("nonempty legacy fixture");
    let legacy_lease = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "legacy-nonempty-root".to_string(),
                actor_id: "root:legacy-nonempty-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec!["legacy.txt".to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("nonempty inline lease starts");
    let pool = coordination_pool(&fixture).await;
    let (legacy_json, legacy_kind) = sqlx::query_as::<_, (String, String)>(
        "SELECT expected_manifest_json, expected_manifest_storage_kind
         FROM workspace_mutation_leases WHERE lease_id = ?",
    )
    .bind(&legacy_lease.lease_id)
    .fetch_one(&pool)
    .await
    .expect("nonempty inline manifest reads");
    assert_eq!(legacy_kind, "inline_v1");
    assert_eq!(
        serde_json::from_str::<Vec<WorkspaceManifestEntry>>(&legacy_json)
            .expect("legacy array decodes")
            .len(),
        1
    );
    fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), legacy_lease)
        .await
        .expect("nonempty legacy array decodes through authoritative codec");

    std::fs::create_dir(fixture.repo.path().join("empty-directory")).expect("empty fixture");
    let empty_lease = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "legacy-empty-root".to_string(),
                actor_id: "root:legacy-empty-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec!["empty-directory".to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("empty inline lease starts");
    let referenced = sqlx::query_scalar::<_, String>(
        "SELECT expected_manifest_json
         FROM workspace_mutation_leases WHERE lease_id = ?",
    )
    .bind(&empty_lease.lease_id)
    .fetch_one(&pool)
    .await
    .expect("referenced empty manifest reads");
    assert_eq!(referenced, "[]");
    let workspace_id = empty_lease.workspace_id.clone();
    fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), empty_lease)
        .await
        .expect("genuinely empty legacy array decodes");

    let error = crate::manifest_storage::decode_manifest(
        &pool,
        &workspace_id,
        r#"{"storage":"content_addressed_v1"}"#,
        "unknown_v1",
        Some("bad"),
    )
    .await
    .expect_err("unknown storage kind must fail closed");
    assert!(matches!(error, StoreError::CorruptData(_)));

    for malformed in [
        r#"{}"#,
        r#"{"storage":"future","tag_version":1,"payload_format_version":1,"manifest_id":"bad"}"#,
        r#"{"storage":"content_addressed","tag_version":2,"payload_format_version":1,"manifest_id":"bad"}"#,
        r#"{"storage":"content_addressed","tag_version":1,"payload_format_version":2,"manifest_id":"bad"}"#,
        r#"{"storage":"content_addressed","tag_version":1,"payload_format_version":1,"manifest_id":"other"}"#,
        r#"{"storage":"content_addressed","tag_version":1,"payload_format_version":1,"manifest_id":"bad","future":true}"#,
    ] {
        let error = crate::manifest_storage::decode_manifest(
            &pool,
            &workspace_id,
            malformed,
            crate::manifest_storage::CONTENT_ADDRESSED_V1,
            Some("bad"),
        )
        .await
        .expect_err("malformed or future references must fail closed");
        assert!(matches!(error, StoreError::CorruptData(_)));
    }
    pool.close().await;
}

#[tokio::test]
async fn exact_two_file_manifest_examines_only_two_paths() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.repo.path().join("one.txt"), "one").expect("first fixture");
    std::fs::write(fixture.repo.path().join("two.txt"), "two").expect("second fixture");
    std::fs::write(fixture.repo.path().join("ignored.txt"), "ignored").expect("ignored fixture");
    let prepared = fixture
        .store
        .prepare_workspace_mutation(
            fixture.repo.path(),
            vec!["one.txt".to_string(), "two.txt".to_string()],
        )
        .await
        .expect("exact manifest prepares")
        .expect("local store supports preparation");
    assert_eq!(prepared.receipt().paths(), ["one.txt", "two.txt"]);
    assert_eq!(prepared.receipt().entries().len(), 2);
    assert_eq!(prepared.work().files_examined, 2);
    assert_eq!(prepared.work().files_hashed, 2);
    assert_eq!(prepared.work().overlay_traversals, 0);
    assert_eq!(prepared.work().git_subprocesses, 0);
}

#[tokio::test]
async fn repository_manifest_workflow_benchmark_1111_files() {
    async fn run_workflow(optimized: bool) -> serde_json::Value {
        const COMMANDS: u64 = 12;
        const OVERLAY_FILES: u64 = 1_111;

        let fixture = Fixture::new().await;
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(fixture.repo.path())
                .output()
                .expect("git benchmark command runs");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Codex Benchmark"]);
        git(&["config", "user.email", "codex-benchmark@example.com"]);
        std::fs::write(fixture.repo.path().join("tracked.txt"), "tracked\n")
            .expect("tracked fixture");
        git(&["add", "tracked.txt"]);
        git(&["commit", "-qm", "benchmark baseline"]);
        for index in 0..OVERLAY_FILES {
            std::fs::write(
                fixture.repo.path().join(format!("overlay-{index:04}.txt")),
                format!("overlay payload {index:04}\n"),
            )
            .expect("overlay fixture");
        }

        let mut cached = if optimized {
            fixture
                .store
                .prepare_workspace_mutation(
                    fixture.repo.path(),
                    vec![REPOSITORY_WIDE_PATH.to_string()],
                )
                .await
                .expect("warm manifest prepares")
        } else {
            None
        };
        let warm_work = cached
            .as_ref()
            .map(super::model::PreparedWorkspaceManifest::work);
        let mut inline_bytes_per_manifest = cached
            .as_ref()
            .map(|manifest| {
                serde_json::to_vec(manifest.receipt().entries())
                    .expect("manifest serializes")
                    .len() as u64
            })
            .unwrap_or(0);

        let mut admission = WorkspaceManifestWork::default();
        let mut final_verification = WorkspaceManifestWork::default();
        let mut admission_wall_micros = 0_u64;
        let mut final_wall_micros = 0_u64;
        for command in 0..COMMANDS {
            let admission_started = std::time::Instant::now();
            let prepared = if optimized {
                let prepared = cached.clone().expect("cached receipt");
                assert_eq!(
                    fixture
                        .store
                        .workspace_mutation_epoch(fixture.repo.path())
                        .await
                        .expect("epoch reads"),
                    Some(prepared.receipt().epoch())
                );
                git(&["config", "--includes", "--show-origin", "--null", "--list"]);
                git(&["rev-parse", "HEAD"]);
                let before = crate::workspace::repository_overlay_paths(fixture.repo.path())
                    .await
                    .expect("authoritative overlay enumerates");
                for path in &before {
                    let file = std::fs::File::open(fixture.repo.path().join(path))
                        .expect("overlay identity opens");
                    file.metadata().expect("overlay identity reads");
                }
                let after = crate::workspace::repository_overlay_paths(fixture.repo.path())
                    .await
                    .expect("authoritative overlay re-enumerates");
                git(&["config", "--includes", "--show-origin", "--null", "--list"]);
                git(&["rev-parse", "HEAD"]);
                assert_eq!(before, after);
                assert_eq!(before.len() as u64, OVERLAY_FILES);
                prepared.reused_after_validation(2, OVERLAY_FILES, 8, 0)
            } else {
                fixture
                    .store
                    .prepare_workspace_mutation(
                        fixture.repo.path(),
                        vec![REPOSITORY_WIDE_PATH.to_string()],
                    )
                    .await
                    .expect("legacy admission manifest prepares")
                    .expect("local store supports preparation")
            };
            let work = prepared.work();
            admission.overlay_traversals = admission
                .overlay_traversals
                .saturating_add(work.overlay_traversals);
            admission.files_examined = admission.files_examined.saturating_add(work.files_examined);
            admission.files_hashed = admission.files_hashed.saturating_add(work.files_hashed);
            admission.bytes_hashed = admission.bytes_hashed.saturating_add(work.bytes_hashed);
            admission.git_subprocesses = admission
                .git_subprocesses
                .saturating_add(work.git_subprocesses);
            admission.manifests_constructed = admission
                .manifests_constructed
                .saturating_add(work.manifests_constructed);
            admission.reuse_hits = admission.reuse_hits.saturating_add(work.reuse_hits);
            if inline_bytes_per_manifest == 0 {
                inline_bytes_per_manifest = serde_json::to_vec(prepared.receipt().entries())
                    .expect("legacy manifest serializes")
                    .len() as u64;
            }
            let lease = fixture
                .store
                .begin_workspace_mutation_prepared(
                    fixture.repo.path(),
                    WorkspaceMutationRequest {
                        root_session_id: format!("benchmark-root-{command}"),
                        actor_id: format!("root:benchmark-{command}"),
                        kind: WorkspaceActorKind::Root,
                        attempt_id: None,
                        paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                        contracts: Vec::new(),
                        expected_manifest: Vec::new(),
                    },
                    prepared,
                )
                .await
                .expect("benchmark lease starts");
            admission_wall_micros = admission_wall_micros.saturating_add(
                admission_started
                    .elapsed()
                    .as_micros()
                    .try_into()
                    .unwrap_or(u64::MAX),
            );

            let final_started = std::time::Instant::now();
            let outcome = fixture
                .store
                .finish_workspace_mutation_with_receipt(fixture.repo.path(), lease)
                .await
                .expect("benchmark lease finalizes");
            let work = outcome.work();
            final_verification.overlay_traversals = final_verification
                .overlay_traversals
                .saturating_add(work.overlay_traversals);
            final_verification.files_examined = final_verification
                .files_examined
                .saturating_add(work.files_examined);
            final_verification.files_hashed = final_verification
                .files_hashed
                .saturating_add(work.files_hashed);
            final_verification.bytes_hashed = final_verification
                .bytes_hashed
                .saturating_add(work.bytes_hashed);
            final_verification.git_subprocesses = final_verification
                .git_subprocesses
                .saturating_add(work.git_subprocesses);
            final_verification.manifests_constructed = final_verification
                .manifests_constructed
                .saturating_add(work.manifests_constructed);
            final_verification.unique_payloads = final_verification
                .unique_payloads
                .saturating_add(work.unique_payloads);
            final_verification.payload_reuses = final_verification
                .payload_reuses
                .saturating_add(work.payload_reuses);
            final_verification.manifest_bytes_persisted = final_verification
                .manifest_bytes_persisted
                .saturating_add(work.manifest_bytes_persisted);
            final_verification.reference_bytes_persisted = final_verification
                .reference_bytes_persisted
                .saturating_add(work.reference_bytes_persisted);
            final_wall_micros = final_wall_micros.saturating_add(
                final_started
                    .elapsed()
                    .as_micros()
                    .try_into()
                    .unwrap_or(u64::MAX),
            );
            if optimized {
                cached = outcome.final_manifest().cloned();
            }
        }

        let pool = coordination_pool(&fixture).await;
        let (payload_count, payload_bytes) = sqlx::query_as::<_, (i64, i64)>(
            "SELECT COUNT(*), COALESCE(SUM(payload_byte_count), 0)
             FROM workspace_manifest_payloads",
        )
        .fetch_one(&pool)
        .await
        .expect("payload metrics read");
        let reference_bytes = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(LENGTH(expected_manifest_json)), 0)
             FROM workspace_mutation_leases",
        )
        .fetch_one(&pool)
        .await
        .expect("reference metrics read");
        pool.close().await;

        serde_json::json!({
            "commands": COMMANDS,
            "overlay_files": OVERLAY_FILES,
            "warm": warm_work.map(|work| serde_json::json!({
                "traversals": work.overlay_traversals,
                "files": work.files_hashed,
                "bytes": work.bytes_hashed,
                "git": work.git_subprocesses,
                "constructions": work.manifests_constructed,
            })),
            "admission": {
                "traversals": admission.overlay_traversals,
                "files_examined": admission.files_examined,
                "files": admission.files_hashed,
                "bytes": admission.bytes_hashed,
                "git": admission.git_subprocesses,
                "constructions": admission.manifests_constructed,
                "hits": admission.reuse_hits,
                "wall_micros": admission_wall_micros,
            },
            "final": {
                "traversals": final_verification.overlay_traversals,
                "files": final_verification.files_hashed,
                "bytes": final_verification.bytes_hashed,
                "git": final_verification.git_subprocesses,
                "constructions": final_verification.manifests_constructed,
                "unique_payloads": final_verification.unique_payloads,
                "payload_reuses": final_verification.payload_reuses,
                "manifest_bytes": final_verification.manifest_bytes_persisted,
                "reference_bytes": final_verification.reference_bytes_persisted,
                "wall_micros": final_wall_micros,
            },
            "stored": {
                "unique_payloads": payload_count,
                "manifest_bytes": payload_bytes,
                "lease_reference_bytes": reference_bytes,
                "counterfactual_inline_admission_bytes": inline_bytes_per_manifest.saturating_mul(COMMANDS),
            }
        })
    }

    let legacy = run_workflow(false).await;
    let optimized = run_workflow(true).await;
    println!(
        "KD4_MANIFEST_BENCHMARK={}",
        serde_json::json!({"legacy": legacy, "optimized": optimized})
    );
    assert_eq!(optimized["admission"]["traversals"], 24);
    assert_eq!(optimized["admission"]["files_examined"], 13_332);
    assert_eq!(optimized["admission"]["files"], 0);
    assert_eq!(optimized["admission"]["bytes"], 0);
    assert_eq!(optimized["admission"]["constructions"], 0);
    assert_eq!(optimized["admission"]["hits"], 12);
    assert_eq!(
        legacy["final"]["traversals"],
        optimized["final"]["traversals"]
    );
    assert_eq!(legacy["final"]["files"], optimized["final"]["files"]);
}

#[tokio::test]
async fn missing_corrupt_and_conflicting_manifest_payloads_fail_closed() {
    async fn referenced_lease(fixture: &Fixture, suffix: &str) -> WorkspaceMutationLease {
        std::fs::write(fixture.repo.path().join("overlay.txt"), suffix).expect("overlay fixture");
        fixture
            .store
            .begin_workspace_mutation(
                fixture.repo.path(),
                WorkspaceMutationRequest {
                    root_session_id: format!("payload-{suffix}"),
                    actor_id: format!("root:payload-{suffix}"),
                    kind: WorkspaceActorKind::Root,
                    attempt_id: None,
                    paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                    contracts: Vec::new(),
                    expected_manifest: Vec::new(),
                },
            )
            .await
            .expect("referenced lease starts")
    }

    let missing = Fixture::new().await;
    let missing_lease = referenced_lease(&missing, "missing").await;
    let pool = coordination_pool(&missing).await;
    sqlx::query("DELETE FROM workspace_manifest_payloads WHERE workspace_id = ?")
        .bind(&missing_lease.workspace_id)
        .execute(&pool)
        .await
        .expect("missing payload injected");
    let error = missing
        .store
        .finish_workspace_mutation(missing.repo.path(), missing_lease)
        .await
        .expect_err("missing payload must fail closed");
    assert!(matches!(error, StoreError::CorruptData(_)));
    pool.close().await;

    let corrupt = Fixture::new().await;
    let corrupt_lease = referenced_lease(&corrupt, "corrupt").await;
    let pool = coordination_pool(&corrupt).await;
    sqlx::query(
        "UPDATE workspace_manifest_payloads
         SET canonical_manifest_bytes = X'5B5D'
         WHERE workspace_id = ?",
    )
    .bind(&corrupt_lease.workspace_id)
    .execute(&pool)
    .await
    .expect("payload corruption injected");
    let error = corrupt
        .store
        .finish_workspace_mutation(corrupt.repo.path(), corrupt_lease)
        .await
        .expect_err("corrupt payload must fail closed");
    assert!(matches!(error, StoreError::CorruptData(_)));
    pool.close().await;

    let conflict = Fixture::new().await;
    std::fs::write(conflict.repo.path().join("overlay.txt"), "conflict").expect("conflict fixture");
    let prepared = conflict
        .store
        .prepare_workspace_mutation(conflict.repo.path(), vec![REPOSITORY_WIDE_PATH.to_string()])
        .await
        .expect("manifest prepares")
        .expect("local store supports preparation");
    let pool = coordination_pool(&conflict).await;
    sqlx::query(
        "INSERT INTO workspace_manifest_payloads (
            workspace_id, manifest_id, payload_format_version,
            canonical_manifest_bytes, entry_count, payload_byte_count, created_at
         ) VALUES (?, ?, 1, X'5B5D', 0, 2, ?)",
    )
    .bind(prepared.receipt().workspace_id())
    .bind(prepared.receipt().manifest_id())
    .bind(serde_json::to_string(&Utc::now()).expect("time serializes"))
    .execute(&pool)
    .await
    .expect("conflicting identity injected");
    let mut transaction = pool.begin().await.expect("transaction begins");
    let error = crate::manifest_storage::encode_canonical_manifest_reference(
        &mut transaction,
        prepared.receipt().workspace_id(),
        &prepared.canonical,
    )
    .await
    .expect_err("conflicting bytes for one hash fail closed");
    assert!(matches!(error, StoreError::CorruptData(_)));
    transaction
        .rollback()
        .await
        .expect("transaction rolls back");
    pool.close().await;
}

async fn expire_workspace_actor_leases(fixture: &Fixture, attempt_ids: &[AttemptId]) {
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
    let stale_at = Utc::now() - Duration::seconds(DEFAULT_WORKSPACE_LEASE_SECONDS + 1);
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
}

async fn expire_workspace_finalization_fence(fixture: &Fixture, fence_id: &str) {
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
    let expired_at = Utc::now() - Duration::seconds(1);
    let encoded_expired_at =
        serde_json::to_string(&expired_at).expect("expired fence time serializes");
    let updated = sqlx::query(
        "UPDATE workspace_finalization_fences
         SET expires_at = ?
         WHERE fence_id = ? AND state IN ('active', 'dispatching')",
    )
    .bind(encoded_expired_at)
    .bind(fence_id)
    .execute(&pool)
    .await
    .expect("workspace finalization fence expires");
    assert_eq!(updated.rows_affected(), 1);
    pool.close().await;
}

async fn workspace_finalization_fence_state(fixture: &Fixture, fence_id: &str) -> String {
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
    let state = sqlx::query_scalar::<_, String>(
        "SELECT state FROM workspace_finalization_fences WHERE fence_id = ?",
    )
    .bind(fence_id)
    .fetch_one(&pool)
    .await
    .expect("workspace finalization fence state reads");
    pool.close().await;
    state
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

fn resolved_test_executable() -> Option<String> {
    Some(
        std::fs::canonicalize(std::env::current_exe().expect("current test executable"))
            .expect("current test executable canonicalizes")
            .to_string_lossy()
            .into_owned(),
    )
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
            resolved_executable: resolved_test_executable(),
            proof_kind: ValidationProofKind::Focused,
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
        .begin_mutation(
            attempt_id,
            repo_root,
            path.to_string(),
            AttributionConfidence::Definitive,
        )
        .await
        .expect("typed mutation evidence starts");
    let lease = store
        .begin_workspace_mutation(
            repo_root,
            WorkspaceMutationRequest {
                root_session_id: root_session_id.to_string(),
                actor_id: format!("attempt:{attempt_id}"),
                kind: WorkspaceActorKind::Typed,
                attempt_id: Some(attempt_id),
                paths: vec![path.to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("workspace mutation lease starts");
    std::fs::write(repo_root.join(path), contents).expect("controlled file write");
    store
        .finish_workspace_mutation(repo_root, lease)
        .await
        .expect("workspace mutation lease finishes");
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
async fn selective_admission_allows_shared_reads_and_rejects_conflicting_ownership() {
    let fixture = Fixture::new().await;
    let root_session_id = "selective-overlap-root";
    let first = fixture
        .store
        .create_admitted_assignment(
            fixture.repo.path(),
            selective_worker_draft(
                root_session_id,
                "src/first.rs",
                &["AGENTS.md", "src/types.rs"],
            ),
            true,
        )
        .await
        .expect("first disjoint writer is admitted");
    assert_eq!(first.integration_plan, IntegrationPlan::SingleWriter);

    let second = fixture
        .store
        .create_admitted_assignment(
            fixture.repo.path(),
            selective_worker_draft(
                root_session_id,
                "src/second.rs",
                &["AGENTS.md", "src/types.rs"],
            ),
            true,
        )
        .await
        .expect("shared supporting reads do not exclude a disjoint writer");
    assert_eq!(second.integration_plan, IntegrationPlan::RootOwned);
    assert_eq!(second.overlaps.benign_read_overlap_count, 1);

    let write_conflict = fixture
        .store
        .create_admitted_assignment(
            fixture.repo.path(),
            selective_worker_draft(root_session_id, "src", &["AGENTS.md"]),
            true,
        )
        .await
        .expect_err("overlapping active writers are rejected");
    assert!(matches!(
        write_conflict,
        StoreError::WriteClaimConflict { .. }
    ));

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
    let read_conflict = sensitive
        .store
        .create_admitted_assignment(
            sensitive.repo.path(),
            selective_worker_draft(root_session_id, "src/critical.rs", &[]),
            true,
        )
        .await
        .expect_err("writes cannot invalidate an active primary investigation");
    assert!(matches!(
        read_conflict,
        StoreError::AdmissionRejected {
            reason: AdmissionRejectionReason::CorrectnessSensitiveReadConflict
        }
    ));
}

#[tokio::test]
async fn explorer_identity_rejects_only_the_same_primary_question() {
    let fixture = Fixture::new().await;
    let root_session_id = "explorer-identity-root";
    let first = explorer_draft(
        root_session_id,
        "src/shared.rs",
        "trace the parser ownership",
    );
    fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), first.clone(), true)
        .await
        .expect("first investigation is admitted");
    let duplicate = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), first, true)
        .await
        .expect_err("the same canonical investigation is rejected");
    assert!(matches!(
        duplicate,
        StoreError::AdmissionRejected {
            reason: AdmissionRejectionReason::DuplicateExplorerInvestigation
        }
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
}

#[tokio::test]
async fn selective_multi_writer_admission_records_the_required_integration_plan() {
    let fixture = Fixture::new().await;
    let root_session_id = "integration-plan-root";
    fixture
        .store
        .create_admitted_assignment(
            fixture.repo.path(),
            selective_worker_draft(root_session_id, "src/first.rs", &[]),
            true,
        )
        .await
        .expect("first writer is admitted");

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
            reason: AdmissionRejectionReason::IsolatedIntegratorUnavailable
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
}

#[tokio::test]
async fn selective_admission_rejects_writes_over_active_verification_proof_ownership() {
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

    let error = fixture
        .store
        .create_admitted_assignment(
            fixture.repo.path(),
            selective_worker_draft(root_session_id, "src/verified.rs", &[]),
            true,
        )
        .await
        .expect_err("writes cannot invalidate an active verification proof");
    assert!(matches!(
        error,
        StoreError::AdmissionRejected {
            reason: AdmissionRejectionReason::ActiveValidationConflict
        }
    ));
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
    let error = fixture
        .store
        .validate_dependencies(AssignmentId::new(), &[incomplete.assignment_id, unknown])
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
    fixture
        .store
        .record_validation_call(ValidationCall {
            call_id: "call-1".to_string(),
            attempt_id: first_attempt.attempt_id,
            command_summary: "focused test".to_string(),
            resolved_executable: resolved_test_executable(),
            proof_kind: ValidationProofKind::Focused,
            evidence: ValidationEvidence::default(),
            status: ValidationCallStatus::Running,
            recorded_at: Utc::now(),
        })
        .await
        .expect("validation call starts");
    fixture
        .store
        .record_validation_call(ValidationCall {
            call_id: "call-1".to_string(),
            attempt_id: first_attempt.attempt_id,
            command_summary: "focused test".to_string(),
            resolved_executable: resolved_test_executable(),
            proof_kind: ValidationProofKind::Focused,
            evidence: ValidationEvidence::default(),
            status: ValidationCallStatus::Succeeded,
            recorded_at: Utc::now(),
        })
        .await
        .expect("validation call finishes");
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
async fn unchanged_missing_evidence_replays_with_stable_assignment_bound_obligations() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("missing-replay-root", "src");
    draft.required_evidence = vec![
        "cargo test -p first".to_string(),
        "cargo test -p second".to_string(),
    ];
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("worker assignment");
    bind_test_agent(
        &fixture.store,
        assignment.assignment_id,
        attempt.attempt_id,
        "missing-replay-root",
    )
    .await;
    let receipt = completed_receipt(Vec::new());
    let error = fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, receipt.clone())
        .await
        .expect_err("missing evidence is rejected");
    let StoreError::RequiredEvidenceMissing { obligations } = error else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(obligations.len(), 2);
    assert!(
        obligations[0]
            .id
            .contains(&assignment.assignment_id.to_string())
    );
    assert!(obligations[0].id.contains(":0001:"));
    assert!(obligations[1].id.contains(":0002:"));

    let replayed = fixture
        .store
        .replay_required_evidence_missing(attempt.attempt_id, &receipt)
        .await
        .expect("cache lookup succeeds")
        .expect("unchanged rejection replays");
    assert_eq!(replayed, obligations);

    let mut changed_receipt = receipt.clone();
    changed_receipt.summary.push_str(" with a changed draft");
    assert_eq!(
        fixture
            .store
            .replay_required_evidence_missing(attempt.attempt_id, &changed_receipt)
            .await
            .expect("changed draft lookup succeeds"),
        None,
        "the complete receipt draft participates in the fingerprint"
    );
}

#[tokio::test]
async fn validation_revision_invalidates_partial_missing_evidence_replay() {
    let fixture = Fixture::new().await;
    let first_command = "cargo test -p first";
    let second_command = "cargo test -p second";
    let mut draft = worker_draft("partial-replay-root", "src");
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
        "partial-replay-root",
    )
    .await;
    let first = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "partial-first",
        first_command,
    )
    .await;
    finish_focused_validation(&fixture.store, first).await;
    let receipt = completed_receipt(vec!["partial-first".to_string()]);
    let error = fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, receipt.clone())
        .await
        .expect_err("second obligation is missing");
    let StoreError::RequiredEvidenceMissing { obligations } = error else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(obligations.len(), 1);
    assert_eq!(obligations[0].requirement, second_command);
    assert_eq!(
        fixture
            .store
            .replay_required_evidence_missing(attempt.attempt_id, &receipt)
            .await
            .expect("partial cache lookup succeeds"),
        Some(obligations),
        "partial replay refreshes referenced validation before the exact hit"
    );

    let _second = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "partial-second",
        second_command,
    )
    .await;
    assert_eq!(
        fixture
            .store
            .replay_required_evidence_missing(attempt.attempt_id, &receipt)
            .await
            .expect("revision-invalidated lookup succeeds"),
        None,
        "every validation-call transition advances the checked revision"
    );
}

#[tokio::test]
async fn partial_missing_evidence_replay_refreshes_host_staleness() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "before\n").expect("source fixture");
    let first_command = "cargo test -p first";
    let second_command = "cargo test -p second";
    let mut draft = worker_draft("partial-stale-root", "src/lib.rs");
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
        "partial-stale-root",
    )
    .await;
    let first = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "partial-stale-first",
        first_command,
    )
    .await;
    finish_focused_validation(&fixture.store, first).await;
    let receipt = completed_receipt(vec!["partial-stale-first".to_string()]);
    let error = fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, receipt.clone())
        .await
        .expect_err("second obligation is missing");
    assert!(matches!(error, StoreError::RequiredEvidenceMissing { .. }));

    std::fs::write(fixture.repo.path().join("src/lib.rs"), "changed\n").expect("host mutation");
    assert_eq!(
        fixture
            .store
            .replay_required_evidence_missing(attempt.attempt_id, &receipt)
            .await
            .expect("staleness-aware lookup succeeds"),
        None
    );
    assert_eq!(
        fixture
            .store
            .get_validation_call("partial-stale-first".to_string())
            .await
            .expect("validation reads")
            .expect("validation exists")
            .status,
        ValidationCallStatus::Superseded
    );
}

#[tokio::test]
async fn missing_evidence_cache_clears_on_rebinding_and_sealing() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("cache-lifecycle-root", "src");
    draft.required_evidence = vec!["cargo test -p missing".to_string()];
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("worker assignment");
    bind_test_agent(
        &fixture.store,
        assignment.assignment_id,
        attempt.attempt_id,
        "cache-lifecycle-root",
    )
    .await;
    let receipt = completed_receipt(Vec::new());
    let error = fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, receipt.clone())
        .await
        .expect_err("missing evidence is rejected");
    assert!(matches!(error, StoreError::RequiredEvidenceMissing { .. }));
    assert!(
        fixture
            .store
            .has_cached_missing_evidence_rejection(attempt.attempt_id)
    );

    bind_test_agent(
        &fixture.store,
        assignment.assignment_id,
        attempt.attempt_id,
        "cache-lifecycle-root",
    )
    .await;
    assert!(
        !fixture
            .store
            .has_cached_missing_evidence_rejection(attempt.attempt_id)
    );

    let error = fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, receipt)
        .await
        .expect_err("missing evidence is rejected again");
    assert!(matches!(error, StoreError::RequiredEvidenceMissing { .. }));
    fixture
        .store
        .abandon_agent_task(
            TaskActor::Root,
            assignment.assignment_id,
            "root replaces the active attempt".to_string(),
        )
        .await
        .expect("attempt seals as abandoned");
    assert!(
        !fixture
            .store
            .has_cached_missing_evidence_rejection(attempt.attempt_id)
    );
}

#[tokio::test]
async fn validation_evidence_revision_is_monotonic() {
    let fixture = Fixture::new().await;
    let command = "cargo test -p revision";
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft("revision-root", "src", command),
        )
        .await
        .expect("worker assignment");
    bind_test_agent(
        &fixture.store,
        assignment.assignment_id,
        attempt.attempt_id,
        "revision-root",
    )
    .await;
    let pool = coordination_pool(&fixture).await;
    assert_eq!(
        validation_evidence_revision(&pool, attempt.attempt_id).await,
        0
    );
    let call =
        start_focused_validation(&fixture.store, attempt.attempt_id, "revision-call", command)
            .await;
    let after_creation = validation_evidence_revision(&pool, attempt.attempt_id).await;
    assert!(after_creation > 0);
    finish_focused_validation(&fixture.store, call).await;
    assert!(validation_evidence_revision(&pool, attempt.attempt_id).await > after_creation);
    pool.close().await;
}

#[test]
fn unsupported_required_evidence_source_disables_replay_cache() {
    assert!(super::local::required_evidence_sources_are_revisioned(&[
        "focused_validation_call_summary:v1"
    ]));
    assert!(!super::local::required_evidence_sources_are_revisioned(&[
        "focused_validation_call_summary:v1",
        "external_evidence:v1",
    ]));
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
    assert!(matches!(
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: "missing-provenance".to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "focused test".to_string(),
                resolved_executable: None,
                proof_kind: ValidationProofKind::Focused,
                evidence: ValidationEvidence::default(),
                status: ValidationCallStatus::Running,
                recorded_at: started_at,
            })
            .await,
        Err(StoreError::InvalidAssignment(_))
    ));
    assert!(matches!(
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: "missing-start".to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "focused test".to_string(),
                resolved_executable: resolved_test_executable(),
                proof_kind: ValidationProofKind::Focused,
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
                resolved_executable: resolved_test_executable(),
                proof_kind: ValidationProofKind::Focused,
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
            resolved_executable: resolved_test_executable(),
            proof_kind: ValidationProofKind::Focused,
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
                command_summary: "focused test".to_string(),
                resolved_executable: resolved_test_executable(),
                proof_kind: ValidationProofKind::LegacyUnclassified,
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
            resolved_executable: resolved_test_executable(),
            proof_kind: ValidationProofKind::Focused,
            evidence: ValidationEvidence::default(),
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
                resolved_executable: resolved_test_executable(),
                proof_kind: ValidationProofKind::Focused,
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
                resolved_executable: resolved_test_executable(),
                proof_kind: ValidationProofKind::Focused,
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
                resolved_executable: resolved_test_executable(),
                proof_kind: ValidationProofKind::Focused,
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
    assert_eq!(task.validation_calls.len(), 4);
    assert_eq!(
        task.validation_calls
            .iter()
            .map(|call| call.call_id.as_str())
            .collect::<HashSet<_>>()
            .len(),
        4
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
                resolved_executable: resolved_test_executable(),
                proof_kind: ValidationProofKind::Focused,
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
                resolved_executable: resolved_test_executable(),
                proof_kind: ValidationProofKind::Focused,
                evidence: ValidationEvidence::default(),
                status: ValidationCallStatus::Running,
                recorded_at: Utc::now(),
            })
            .await,
        Err(StoreError::InvalidAssignment(_))
    ));
}

#[tokio::test]
async fn legacy_unclassified_validation_defaults_on_old_json_and_cannot_complete() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("root", "src");
    draft.required_evidence = vec!["focused test".to_string()];
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("worker assignment");
    let focused = ValidationCall {
        call_id: "legacy".to_string(),
        attempt_id: attempt.attempt_id,
        command_summary: "focused test".to_string(),
        resolved_executable: resolved_test_executable(),
        proof_kind: ValidationProofKind::Focused,
        evidence: ValidationEvidence::default(),
        status: ValidationCallStatus::Running,
        recorded_at: Utc::now(),
    };
    let mut old_json = serde_json::to_value(focused).expect("validation call serializes");
    old_json
        .as_object_mut()
        .expect("validation call is an object")
        .remove("proof_kind");
    old_json
        .as_object_mut()
        .expect("validation call is an object")
        .remove("resolved_executable");
    let legacy: ValidationCall =
        serde_json::from_value(old_json).expect("old validation call JSON remains readable");
    assert_eq!(legacy.proof_kind, ValidationProofKind::LegacyUnclassified);
    fixture
        .store
        .record_validation_call(legacy.clone())
        .await
        .expect("legacy validation call starts");
    fixture
        .store
        .record_validation_call(ValidationCall {
            status: ValidationCallStatus::Succeeded,
            recorded_at: legacy.recorded_at + Duration::seconds(1),
            ..legacy
        })
        .await
        .expect("legacy validation call finishes");

    assert!(matches!(
        fixture
            .store
            .submit_agent_receipt(
                attempt.attempt_id,
                completed_receipt(vec!["legacy".to_string()])
            )
            .await,
        Err(StoreError::ValidationCallStatusInvalid { .. })
    ));
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
        .expect("released write claim allows a retry assignment");
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
        .expect("correction atomically reacquires the write claim");
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
    assert!(matches!(
        fixture
            .store
            .create_assignment(
                fixture.repo.path(),
                worker_draft("risk-root", "src/file.rs")
            )
            .await,
        Err(StoreError::WriteClaimConflict { .. })
    ));

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
    fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("risk-root", "src/file.rs"),
        )
        .await
        .expect("claim releases only after verification passes");
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
    expire_workspace_actor_leases(&fixture, &[attempt.attempt_id]).await;

    assert!(
        fixture
            .store
            .heartbeat_typed_workspace_actor(binding.clone())
            .await
            .expect("typed heartbeat")
    );
    let mut mismatched = binding.clone();
    mismatched.thread_id = Some("wrong-thread".to_string());
    assert!(
        !fixture
            .store
            .heartbeat_typed_workspace_actor(mismatched)
            .await
            .expect("mismatched heartbeat is rejected")
    );

    expire_workspace_actor_leases(&fixture, &[attempt.attempt_id]).await;
    let lease = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "heartbeat-root".to_string(),
                actor_id: format!("attempt:{}", attempt.attempt_id),
                kind: WorkspaceActorKind::Typed,
                attempt_id: Some(attempt.attempt_id),
                paths: vec!["src/lib.rs".to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("typed mutation self-renews before orphan scavenging");
    fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), lease)
        .await
        .expect("typed mutation finishes");

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
        !fixture
            .store
            .heartbeat_typed_workspace_actor(binding)
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
    assert!(matches!(
        expired_fixture
            .store
            .begin_workspace_mutation(
                expired_fixture.repo.path(),
                WorkspaceMutationRequest {
                    root_session_id: "expired-owner-root".to_string(),
                    actor_id: "root:expired-owner-root".to_string(),
                    kind: WorkspaceActorKind::Root,
                    attempt_id: None,
                    paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                    contracts: Vec::new(),
                    expected_manifest: Vec::new(),
                }
            )
            .await,
        Err(StoreError::WorkspaceClaimConflict { .. })
    ));

    expire_workspace_actor_leases(&expired_fixture, &[expired_attempt.attempt_id]).await;
    let lease = expired_fixture
        .store
        .begin_workspace_mutation(
            expired_fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "expired-owner-root".to_string(),
                actor_id: "root:expired-owner-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("expired owner no longer blocks a repository-wide mutation");
    expired_fixture
        .store
        .finish_workspace_mutation(expired_fixture.repo.path(), lease)
        .await
        .expect("repository-wide mutation lease releases");
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
    let lease = missing_fixture
        .store
        .begin_workspace_mutation(
            missing_fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "missing-owner-root".to_string(),
                actor_id: "root:missing-owner-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("missing owner record cannot strand a claim");
    missing_fixture
        .store
        .finish_workspace_mutation(missing_fixture.repo.path(), lease)
        .await
        .expect("replacement mutation lease releases");
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

    expire_workspace_actor_leases(&fixture, &[worker_attempt.attempt_id]).await;
    assert!(matches!(
        fixture
            .store
            .create_assignment(
                fixture.repo.path(),
                worker_draft("competing-root", "src/file.rs")
            )
            .await,
        Err(StoreError::WriteClaimConflict { .. })
    ));

    expire_workspace_actor_leases(&fixture, &[reviewer_attempt.attempt_id]).await;
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
    let watermark = first.updated_agents.last().expect("event").event_id;
    let repeated = fixture
        .store
        .read_wake_events("wake-root".to_string(), None)
        .await
        .expect("non-draining reread");
    assert_eq!(first.updated_agents, repeated.updated_agents);

    fixture.store.close().await;
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
            assert!(page.timed_out);
            break;
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
async fn integrator_supersedes_only_targeted_successful_claims() {
    let fixture = Fixture::new().await;
    let (worker, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "shared"))
        .await
        .expect("worker assignment");
    assert!(matches!(
        fixture
            .store
            .create_assignment(fixture.repo.path(), worker_draft("root", "shared/file.rs"))
            .await,
        Err(StoreError::WriteClaimConflict { .. })
    ));
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
        .expect("targeted integrator supersedes retained worker claim");
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
async fn mutation_evidence_keeps_private_prewrite_snapshot() {
    let fixture = Fixture::new().await;
    tokio::fs::create_dir_all(fixture.repo.path().join("src"))
        .await
        .expect("source directory");
    tokio::fs::write(fixture.repo.path().join("src/file.rs"), b"before")
        .await
        .expect("prewrite file");
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "src"))
        .await
        .expect("worker assignment");
    let event_id = fixture
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
    let evidence = fixture
        .store
        .finalize_mutation(
            attempt.attempt_id,
            fixture.repo.path(),
            "src/file.rs".to_string(),
        )
        .await
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
    assert!(matches!(
        fixture
            .store
            .garbage_collect_snapshots(assignment.assignment_id, true)
            .await,
        Err(StoreError::SnapshotRetentionRequired)
    ));
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
    let decoded: Assignment = serde_json::from_value(value).expect("legacy assignment decodes");
    assert!(decoded.repository_id.is_empty());
    assert_eq!(decoded.task_capsule, None);
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
        relevant_handles: Vec::new(),
        workspace_epoch: assignment.start_epoch,
        workspace_manifest_hash: "manifest-sha256".to_string(),
        prohibited_changes: assignment.prohibited_changes.clone(),
        required_evidence: assignment.required_evidence.clone(),
    };
    let pretty = serde_json::to_string_pretty(&capsule).expect("capsule pretty serializes");
    assert!(matches!(
        fixture
            .store
            .attach_task_capsule(assignment.assignment_id, attempt.attempt_id, pretty)
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
async fn relevant_drift_supersedes_validation_but_unrelated_drift_preserves_it() {
    let relevant = Fixture::new().await;
    std::fs::create_dir_all(relevant.repo.path().join("src")).expect("src directory");
    std::fs::write(relevant.repo.path().join("src/a.rs"), "before\n").expect("a fixture");
    std::fs::write(relevant.repo.path().join("src/b.rs"), "before\n").expect("b fixture");
    let command = "cargo test -p owner narrow";
    let (_, relevant_attempt) = relevant
        .store
        .create_assignment(
            relevant.repo.path(),
            validation_worker_draft("relevant-root", "src/a.rs", command),
        )
        .await
        .expect("relevant assignment");
    let relevant_call = finish_focused_validation(
        &relevant.store,
        start_focused_validation(
            &relevant.store,
            relevant_attempt.attempt_id,
            "relevant-validation",
            command,
        )
        .await,
    )
    .await;
    assert_eq!(relevant_call.status, ValidationCallStatus::Succeeded);
    std::fs::write(relevant.repo.path().join("src/a.rs"), "external change\n")
        .expect("relevant external change");
    let error = relevant
        .store
        .submit_agent_receipt(
            relevant_attempt.attempt_id,
            completed_receipt(vec!["relevant-validation".to_string()]),
        )
        .await
        .expect_err("stale relevant proof cannot seal");
    assert!(matches!(error, StoreError::EvidenceSuperseded { .. }));
    assert_eq!(
        relevant
            .store
            .get_validation_call("relevant-validation".to_string())
            .await
            .expect("stale validation reads")
            .expect("stale validation exists")
            .status,
        ValidationCallStatus::Superseded
    );

    let unrelated = Fixture::new().await;
    std::fs::create_dir_all(unrelated.repo.path().join("src")).expect("src directory");
    std::fs::write(unrelated.repo.path().join("src/a.rs"), "before\n").expect("a fixture");
    std::fs::write(unrelated.repo.path().join("src/b.rs"), "before\n").expect("b fixture");
    unrelated
        .store
        .capture_workspace_revision(unrelated.repo.path(), vec!["src/b.rs".to_string()])
        .await
        .expect("unrelated path baseline");
    let (_, unrelated_attempt) = unrelated
        .store
        .create_assignment(
            unrelated.repo.path(),
            validation_worker_draft("unrelated-root", "src/a.rs", command),
        )
        .await
        .expect("unrelated assignment");
    finish_focused_validation(
        &unrelated.store,
        start_focused_validation(
            &unrelated.store,
            unrelated_attempt.attempt_id,
            "unrelated-validation",
            command,
        )
        .await,
    )
    .await;
    std::fs::write(unrelated.repo.path().join("src/b.rs"), "external change\n")
        .expect("unrelated external change");
    unrelated
        .store
        .capture_workspace_revision(unrelated.repo.path(), vec!["src/b.rs".to_string()])
        .await
        .expect("unrelated drift is detected");
    unrelated
        .store
        .submit_agent_receipt(
            unrelated_attempt.attempt_id,
            completed_receipt(vec!["unrelated-validation".to_string()]),
        )
        .await
        .expect("narrow proof survives unrelated drift");
    std::fs::write(
        unrelated.repo.path().join("src/b.rs"),
        "later unrelated change\n",
    )
    .expect("later unrelated external change");
    unrelated
        .store
        .capture_workspace_revision(unrelated.repo.path(), vec!["src/b.rs".to_string()])
        .await
        .expect("later unrelated drift is detected");
    assert!(
        unrelated
            .store
            .check_quiescence("unrelated-root".to_string())
            .await
            .expect("unrelated post-receipt drift remains quiescent")
            .quiescent
    );

    let sealed = Fixture::new().await;
    std::fs::create_dir_all(sealed.repo.path().join("src")).expect("src directory");
    std::fs::write(sealed.repo.path().join("src/lib.rs"), "before\n").expect("lib fixture");
    let (_, sealed_attempt) = sealed
        .store
        .create_assignment(
            sealed.repo.path(),
            validation_worker_draft("sealed-root", "src/lib.rs", command),
        )
        .await
        .expect("sealed assignment");
    finish_focused_validation(
        &sealed.store,
        start_focused_validation(
            &sealed.store,
            sealed_attempt.attempt_id,
            "sealed-validation",
            command,
        )
        .await,
    )
    .await;
    sealed
        .store
        .submit_agent_receipt(
            sealed_attempt.attempt_id,
            completed_receipt(vec!["sealed-validation".to_string()]),
        )
        .await
        .expect("fresh receipt seals");
    std::fs::write(
        sealed.repo.path().join("src/lib.rs"),
        "changed after receipt\n",
    )
    .expect("post-receipt relevant change");
    for _ in 0..2 {
        let result = sealed
            .store
            .check_quiescence("sealed-root".to_string())
            .await;
        assert!(
            matches!(result, Err(StoreError::EvidenceSuperseded { .. })),
            "post-receipt relevant drift persistently blocks root completion: {result:?}"
        );
    }
}

#[tokio::test]
async fn in_flight_validation_supersedes_only_relevant_drift() {
    let command = "cargo test -p owner narrow";

    let disjoint = Fixture::new().await;
    std::fs::create_dir_all(disjoint.repo.path().join("src")).expect("src directory");
    std::fs::write(disjoint.repo.path().join("src/a.rs"), "before\n").expect("a fixture");
    std::fs::write(disjoint.repo.path().join("src/b.rs"), "before\n").expect("b fixture");
    disjoint
        .store
        .capture_workspace_revision(disjoint.repo.path(), vec!["src/b.rs".to_string()])
        .await
        .expect("disjoint baseline");
    let (_, disjoint_attempt) = disjoint
        .store
        .create_assignment(
            disjoint.repo.path(),
            validation_worker_draft("in-flight-disjoint-root", "src/a.rs", command),
        )
        .await
        .expect("disjoint assignment");
    let disjoint_call = start_focused_validation(
        &disjoint.store,
        disjoint_attempt.attempt_id,
        "in-flight-disjoint-validation",
        command,
    )
    .await;
    std::fs::write(disjoint.repo.path().join("src/b.rs"), "changed\n").expect("disjoint mutation");
    disjoint
        .store
        .capture_workspace_revision(disjoint.repo.path(), vec!["src/b.rs".to_string()])
        .await
        .expect("disjoint mutation is observed");
    assert_eq!(
        finish_focused_validation(&disjoint.store, disjoint_call)
            .await
            .status,
        ValidationCallStatus::Succeeded
    );

    let relevant = Fixture::new().await;
    std::fs::create_dir_all(relevant.repo.path().join("src")).expect("src directory");
    std::fs::write(relevant.repo.path().join("src/a.rs"), "before\n").expect("a fixture");
    let (_, relevant_attempt) = relevant
        .store
        .create_assignment(
            relevant.repo.path(),
            validation_worker_draft("in-flight-relevant-root", "src/a.rs", command),
        )
        .await
        .expect("relevant assignment");
    let relevant_call = start_focused_validation(
        &relevant.store,
        relevant_attempt.attempt_id,
        "in-flight-relevant-validation",
        command,
    )
    .await;
    std::fs::write(relevant.repo.path().join("src/a.rs"), "changed\n").expect("relevant mutation");
    assert_eq!(
        finish_focused_validation(&relevant.store, relevant_call)
            .await
            .status,
        ValidationCallStatus::Superseded
    );
}

#[tokio::test]
async fn directory_changes_and_new_build_configuration_supersede_validation() {
    let directory = Fixture::new().await;
    std::fs::create_dir_all(directory.repo.path().join("src")).expect("src directory");
    std::fs::write(directory.repo.path().join("src/lib.rs"), "before\n").expect("lib fixture");
    let command = "cargo test -p owner directory";
    let mut draft = validation_worker_draft("directory-root", "src", command);
    draft.write_scope[0].recursive = true;
    let (_, attempt) = directory
        .store
        .create_assignment(directory.repo.path(), draft)
        .await
        .expect("directory assignment");
    let validation = finish_focused_validation(
        &directory.store,
        start_focused_validation(
            &directory.store,
            attempt.attempt_id,
            "directory-validation",
            command,
        )
        .await,
    )
    .await;
    assert!(
        validation
            .evidence
            .covered_scopes
            .iter()
            .any(|scope| scope.path == "src" && scope.recursive)
    );
    std::fs::write(directory.repo.path().join("src/new.rs"), "added later\n")
        .expect("new relevant source");
    assert!(
        matches!(
            directory
                .store
                .submit_agent_receipt(
                    attempt.attempt_id,
                    completed_receipt(vec!["directory-validation".to_string()])
                )
                .await,
            Err(StoreError::EvidenceSuperseded { .. })
        ),
        "a new file inside a recursively covered scope invalidates the proof"
    );

    let deletion = Fixture::new().await;
    std::fs::create_dir_all(deletion.repo.path().join("src")).expect("src directory");
    std::fs::write(deletion.repo.path().join("src/lib.rs"), "before\n").expect("lib fixture");
    let command = "cargo test -p owner directory-deletion";
    let (_, attempt) = deletion
        .store
        .create_assignment(
            deletion.repo.path(),
            validation_worker_draft("directory-deletion-root", "src", command),
        )
        .await
        .expect("directory deletion assignment");
    let validation = finish_focused_validation(
        &deletion.store,
        start_focused_validation(
            &deletion.store,
            attempt.attempt_id,
            "directory-deletion-validation",
            command,
        )
        .await,
    )
    .await;
    std::fs::remove_file(deletion.repo.path().join("src/lib.rs")).expect("delete relevant source");
    let deletion_revision = deletion
        .store
        .capture_workspace_revision(deletion.repo.path(), vec!["src".to_string()])
        .await
        .expect("recursive deletion drift is detected");
    assert!(
        deletion_revision.epoch > validation.evidence.end_epoch.expect("validation end epoch"),
        "deleting a previously observed child advances the workspace epoch"
    );
    assert!(
        deletion_revision
            .files
            .iter()
            .any(|entry| entry.path == "src/lib.rs" && !entry.existed),
        "the deleted child remains explicit in the covered manifest"
    );
    assert!(
        matches!(
            deletion
                .store
                .submit_agent_receipt(
                    attempt.attempt_id,
                    completed_receipt(vec!["directory-deletion-validation".to_string()])
                )
                .await,
            Err(StoreError::EvidenceSuperseded { .. })
        ),
        "deleting a file inside a recursively covered scope invalidates the proof"
    );

    let build_config = Fixture::new().await;
    std::fs::create_dir_all(build_config.repo.path().join("src")).expect("src directory");
    std::fs::write(build_config.repo.path().join("src/lib.rs"), "before\n").expect("lib fixture");
    let command = "cargo test -p owner build-config";
    let (_, attempt) = build_config
        .store
        .create_assignment(
            build_config.repo.path(),
            validation_worker_draft("build-config-root", "src/lib.rs", command),
        )
        .await
        .expect("build config assignment");
    finish_focused_validation(
        &build_config.store,
        start_focused_validation(
            &build_config.store,
            attempt.attempt_id,
            "build-config-validation",
            command,
        )
        .await,
    )
    .await;
    std::fs::create_dir_all(build_config.repo.path().join("new-crate")).expect("new crate");
    std::fs::write(
        build_config.repo.path().join("new-crate/Cargo.toml"),
        "[package]\nname = \"new-crate\"\n",
    )
    .expect("new build configuration");
    assert!(
        matches!(
            build_config
                .store
                .submit_agent_receipt(
                    attempt.attempt_id,
                    completed_receipt(vec!["build-config-validation".to_string()])
                )
                .await,
            Err(StoreError::EvidenceSuperseded { .. })
        ),
        "new repository build configuration invalidates otherwise narrow proof"
    );
}

#[tokio::test]
async fn root_and_foreign_mutation_leases_block_overlapping_claims() {
    let fixture = Fixture::new().await;
    let root_session_id = "claim-root";
    fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: root_session_id.to_string(),
                actor_id: format!("root:{root_session_id}"),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("root mutation lease");
    assert!(matches!(
        fixture
            .store
            .create_assignment(
                fixture.repo.path(),
                worker_draft(root_session_id, "src/lib.rs"),
            )
            .await,
        Err(StoreError::WorkspaceClaimConflict { .. })
    ));

    let foreign = Fixture::new().await;
    foreign
        .store
        .begin_workspace_mutation(
            foreign.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: root_session_id.to_string(),
                actor_id: "legacy:foreign-writer".to_string(),
                kind: WorkspaceActorKind::Legacy,
                attempt_id: None,
                paths: vec!["src/lib.rs".to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("foreign mutation lease");
    assert!(matches!(
        foreign
            .store
            .create_assignment(
                foreign.repo.path(),
                worker_draft(root_session_id, "src/lib.rs"),
            )
            .await,
        Err(StoreError::WorkspaceClaimConflict { .. })
    ));

    let spoofed = Fixture::new().await;
    spoofed
        .store
        .begin_workspace_mutation(
            spoofed.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: root_session_id.to_string(),
                actor_id: format!("root:{root_session_id}"),
                kind: WorkspaceActorKind::Legacy,
                attempt_id: None,
                paths: vec!["src/lib.rs".to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("spoofed root actor lease");
    assert!(matches!(
        spoofed
            .store
            .create_assignment(
                spoofed.repo.path(),
                worker_draft(root_session_id, "src/lib.rs"),
            )
            .await,
        Err(StoreError::WorkspaceClaimConflict { .. })
    ));
}

#[tokio::test]
async fn same_actor_supersedes_an_overlapping_recovered_mutation_lease() {
    let fixture = Fixture::new().await;
    let request = WorkspaceMutationRequest {
        root_session_id: "recovered-root".to_string(),
        actor_id: "root".to_string(),
        kind: WorkspaceActorKind::Root,
        attempt_id: None,
        paths: vec!["src/local.rs".to_string()],
        contracts: Vec::new(),
        expected_manifest: Vec::new(),
    };
    let stale = fixture
        .store
        .begin_workspace_mutation(fixture.repo.path(), request.clone())
        .await
        .expect("initial mutation lease");

    let recovered = fixture
        .store
        .begin_workspace_mutation(fixture.repo.path(), request.clone())
        .await
        .expect("the same actor can recover an overlapping mutation lease");
    assert_ne!(stale.lease_id, recovered.lease_id);
    assert!(
        !fixture
            .store
            .heartbeat_workspace_mutation(
                fixture.repo.path(),
                stale.lease_id.clone(),
                stale.actor_id.clone(),
            )
            .await
            .expect("stale mutation heartbeat is checked")
    );
    assert!(
        fixture
            .store
            .heartbeat_workspace_mutation(
                fixture.repo.path(),
                recovered.lease_id.clone(),
                recovered.actor_id.clone(),
            )
            .await
            .expect("recovered mutation heartbeat is checked")
    );

    let foreign_error = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                actor_id: "foreign-root".to_string(),
                ..request
            },
        )
        .await
        .expect_err("a different actor remains blocked by the recovered lease");
    assert!(matches!(
        foreign_error,
        StoreError::WorkspaceClaimConflict { .. }
    ));

    fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), recovered)
        .await
        .expect("recovered mutation lease releases cleanly");
}

#[tokio::test]
async fn typed_claims_block_untyped_writers_and_supporting_reads_enforce_cas() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "before\n").expect("lib fixture");
    let mut draft = worker_draft("claim-root", "src/lib.rs");
    draft.contract_claims = vec!["schema-owner".to_string()];
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("claimed assignment");
    bind_test_agent(
        &fixture.store,
        assignment.assignment_id,
        attempt.attempt_id,
        "claim-root",
    )
    .await;
    assert!(
        matches!(
            fixture
                .store
                .assert_workspace_unclaimed(fixture.repo.path(), None)
                .await,
            Err(StoreError::WorkspaceClaimConflict { .. })
        ),
        "root and legacy actors must not bypass a typed claim"
    );
    assert!(
        matches!(
            fixture
                .store
                .begin_workspace_mutation(
                    fixture.repo.path(),
                    WorkspaceMutationRequest {
                        root_session_id: "claim-root".to_string(),
                        actor_id: "root:claim-root".to_string(),
                        kind: WorkspaceActorKind::Root,
                        attempt_id: None,
                        paths: vec!["disjoint.txt".to_string()],
                        contracts: Vec::new(),
                        expected_manifest: Vec::new(),
                    }
                )
                .await,
            Err(StoreError::WorkspaceClaimConflict { .. })
        ),
        "an untyped writer cannot bypass an active named-contract claim on a disjoint path"
    );
    let actor_id = format!("attempt:{}", attempt.attempt_id);
    let supporting = fixture
        .store
        .record_supporting_read(
            fixture.repo.path(),
            actor_id.clone(),
            vec!["src/lib.rs".to_string()],
        )
        .await
        .expect("supporting read is durable");
    assert!(
        matches!(
            fixture
                .store
                .begin_workspace_mutation(
                    fixture.repo.path(),
                    WorkspaceMutationRequest {
                        root_session_id: "claim-root".to_string(),
                        actor_id: actor_id.clone(),
                        kind: WorkspaceActorKind::Typed,
                        attempt_id: Some(attempt.attempt_id),
                        paths: vec!["src/lib.rs".to_string()],
                        contracts: vec!["unclaimed-contract".to_string()],
                        expected_manifest: supporting.files.clone(),
                    }
                )
                .await,
            Err(StoreError::WorkspaceClaimConflict { .. })
        ),
        "a typed writer cannot declare a named contract it does not own"
    );
    std::fs::write(
        fixture.repo.path().join("src/lib.rs"),
        "changed after read\n",
    )
    .expect("external edit");
    let expected_manifest = fixture
        .store
        .supporting_read_manifest(
            fixture.repo.path(),
            actor_id.clone(),
            vec!["src/lib.rs".to_string()],
        )
        .await
        .expect("supporting manifest reads");
    let error = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "claim-root".to_string(),
                actor_id,
                kind: WorkspaceActorKind::Typed,
                attempt_id: Some(attempt.attempt_id),
                paths: vec!["src/lib.rs".to_string()],
                contracts: vec!["schema-owner".to_string()],
                expected_manifest,
            },
        )
        .await
        .expect_err("changed supporting read must fail CAS");
    assert!(matches!(error, StoreError::WorkspaceCasMismatch { .. }));
    let current = fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["src/lib.rs".to_string()])
        .await
        .expect("external drift is persisted");
    assert!(current.epoch > supporting.epoch);
    let events = fixture
        .store
        .read_workspace_events(fixture.repo.path(), supporting.epoch)
        .await
        .expect("workspace drift events read");
    assert!(events.iter().any(|event| {
        event.actor_kind == WorkspaceActorKind::External
            && event.attribution_confidence == AttributionConfidence::DetectionOnly
            && event.paths == vec!["src/lib.rs".to_string()]
    }));
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
        vec!["src/lib.rs"]
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
        vec!["src/lib.rs"]
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
async fn partial_supporting_reads_cannot_bypass_multi_file_cas() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/a.rs"), "a\n").expect("a fixture");
    std::fs::write(fixture.repo.path().join("src/b.rs"), "b\n").expect("b fixture");
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("partial-cas-root", "src"))
        .await
        .expect("partial CAS assignment");
    bind_test_agent(
        &fixture.store,
        assignment.assignment_id,
        attempt.attempt_id,
        "partial-cas-root",
    )
    .await;
    let actor_id = format!("attempt:{}", attempt.attempt_id);
    fixture
        .store
        .record_supporting_read(
            fixture.repo.path(),
            actor_id.clone(),
            vec!["src/a.rs".to_string()],
        )
        .await
        .expect("one supporting read");
    let expected_manifest = fixture
        .store
        .supporting_read_manifest(
            fixture.repo.path(),
            actor_id.clone(),
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
        )
        .await
        .expect("partial supporting manifest");
    let error = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "partial-cas-root".to_string(),
                actor_id,
                kind: WorkspaceActorKind::Typed,
                attempt_id: Some(attempt.attempt_id),
                paths: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
                contracts: Vec::new(),
                expected_manifest,
            },
        )
        .await
        .expect_err("every requested mutation path requires matching read evidence");
    assert!(
        matches!(
            error,
            StoreError::WorkspaceCasMismatch { details }
                if details.iter().map(|detail| detail.path.as_str()).collect::<Vec<_>>()
                    == vec!["src/b.rs"]
        ),
        "the unread path is reported as the CAS mismatch"
    );
}

#[tokio::test]
async fn mutation_finalization_refreshes_the_same_actors_supporting_identity() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "before\n").expect("source fixture");
    let actor_id = "root:self-refresh-root".to_string();
    fixture
        .store
        .record_supporting_read(
            fixture.repo.path(),
            actor_id.clone(),
            vec!["src/lib.rs".to_string()],
        )
        .await
        .expect("supporting read");
    let expected_manifest = fixture
        .store
        .supporting_read_manifest(
            fixture.repo.path(),
            actor_id.clone(),
            vec!["src/lib.rs".to_string()],
        )
        .await
        .expect("supporting manifest");
    let lease = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "self-refresh-root".to_string(),
                actor_id: actor_id.clone(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec!["src/lib.rs".to_string()],
                contracts: Vec::new(),
                expected_manifest,
            },
        )
        .await
        .expect("first mutation begins");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "after\n").expect("owned edit");
    fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), lease)
        .await
        .expect("first mutation finalizes");

    let refreshed_manifest = fixture
        .store
        .supporting_read_manifest(
            fixture.repo.path(),
            actor_id.clone(),
            vec!["src/lib.rs".to_string()],
        )
        .await
        .expect("refreshed supporting manifest");
    let second = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "self-refresh-root".to_string(),
                actor_id,
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec!["src/lib.rs".to_string()],
                contracts: Vec::new(),
                expected_manifest: refreshed_manifest,
            },
        )
        .await
        .expect("same-actor follow-up mutation uses refreshed identity");
    fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), second)
        .await
        .expect("second mutation finalizes");
}

#[tokio::test]
async fn external_drift_after_owned_refresh_reports_structured_identities() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "before\n").expect("source fixture");
    let actor_id = "root:external-after-refresh-root".to_string();
    fixture
        .store
        .record_supporting_read(
            fixture.repo.path(),
            actor_id.clone(),
            vec!["src/lib.rs".to_string()],
        )
        .await
        .expect("supporting read");
    let expected_manifest = fixture
        .store
        .supporting_read_manifest(
            fixture.repo.path(),
            actor_id.clone(),
            vec!["src/lib.rs".to_string()],
        )
        .await
        .expect("supporting manifest");
    let lease = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "external-after-refresh-root".to_string(),
                actor_id: actor_id.clone(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec!["src/lib.rs".to_string()],
                contracts: Vec::new(),
                expected_manifest,
            },
        )
        .await
        .expect("owned mutation begins");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "owned\n").expect("owned edit");
    fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), lease)
        .await
        .expect("owned mutation finalizes");
    let refreshed_manifest = fixture
        .store
        .supporting_read_manifest(
            fixture.repo.path(),
            actor_id.clone(),
            vec!["src/lib.rs".to_string()],
        )
        .await
        .expect("refreshed manifest");

    std::fs::write(fixture.repo.path().join("src/lib.rs"), "external\n").expect("external drift");
    let error = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "external-after-refresh-root".to_string(),
                actor_id,
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec!["src/lib.rs".to_string()],
                contracts: Vec::new(),
                expected_manifest: refreshed_manifest,
            },
        )
        .await
        .expect_err("external drift remains rejected");
    let StoreError::WorkspaceCasMismatch { details } = error else {
        panic!("expected a structured CAS mismatch")
    };
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].path, "src/lib.rs");
    assert_ne!(details[0].expected, details[0].current);
    assert!(details[0].current_epoch.is_some());
}

#[tokio::test]
async fn workspace_mutation_coordination_is_scoped_to_root_session() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "before\n").expect("lib fixture");

    let first_session_lease = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "first-root".to_string(),
                actor_id: "root:first-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("first session mutation starts");

    assert!(
        matches!(
            fixture
                .store
                .begin_workspace_mutation(
                    fixture.repo.path(),
                    WorkspaceMutationRequest {
                        root_session_id: "first-root".to_string(),
                        actor_id: "legacy:first-root".to_string(),
                        kind: WorkspaceActorKind::Legacy,
                        attempt_id: None,
                        paths: vec!["src/lib.rs".to_string()],
                        contracts: Vec::new(),
                        expected_manifest: Vec::new(),
                    },
                )
                .await,
            Err(StoreError::WorkspaceClaimConflict { .. })
        ),
        "overlapping mutations in the same root session still conflict"
    );

    fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("second-root", "src/lib.rs"),
        )
        .await
        .expect("another session's mutation does not block typed assignment claims");

    assert!(
        matches!(
            fixture
                .store
                .begin_workspace_mutation(
                    fixture.repo.path(),
                    WorkspaceMutationRequest {
                        root_session_id: "second-root".to_string(),
                        actor_id: "root:second-root".to_string(),
                        kind: WorkspaceActorKind::Root,
                        attempt_id: None,
                        paths: vec!["src/lib.rs".to_string()],
                        contracts: Vec::new(),
                        expected_manifest: Vec::new(),
                    },
                )
                .await,
            Err(StoreError::WorkspaceClaimConflict { .. })
        ),
        "typed claims still protect their own root session"
    );

    let third_session_lease = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "third-root".to_string(),
                actor_id: "root:third-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec!["src/lib.rs".to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("other sessions do not block overlapping workspace mutations");

    fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), third_session_lease)
        .await
        .expect("third session mutation finishes");
    fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), first_session_lease)
        .await
        .expect("first session mutation finishes");
}

#[tokio::test]
async fn repository_wide_mutation_lease_blocks_claims_heartbeats_and_records_paths() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "before\n").expect("lib fixture");
    let lease = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "root-lease".to_string(),
                actor_id: "root:root-lease".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("root mutation lease starts");
    assert!(
        fixture
            .store
            .heartbeat_workspace_mutation(
                fixture.repo.path(),
                lease.lease_id.clone(),
                lease.actor_id.clone(),
            )
            .await
            .expect("root mutation heartbeat")
    );
    let quiescence = fixture
        .store
        .check_quiescence("root-lease".to_string())
        .await
        .expect("lease quiescence");
    assert!(!quiescence.quiescent);
    assert_eq!(
        quiescence.active_mutation_lease_ids,
        vec![lease.lease_id.clone()]
    );
    assert!(
        matches!(
            fixture
                .store
                .create_assignment(
                    fixture.repo.path(),
                    worker_draft("root-lease", "src/lib.rs")
                )
                .await,
            Err(StoreError::WorkspaceClaimConflict { .. })
        ),
        "a typed claim cannot race a live repository-wide writer"
    );
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "after\n").expect("root edit");
    let result = fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), lease.clone())
        .await
        .expect("root mutation finalizes");
    assert_eq!(result.changed_paths, vec!["src/lib.rs".to_string()]);
    let events = fixture
        .store
        .read_workspace_events(fixture.repo.path(), lease.start_epoch)
        .await
        .expect("root mutation events");
    assert!(events.iter().any(|event| {
        event.actor_id.as_deref() == Some("root:root-lease")
            && event.actor_kind == WorkspaceActorKind::Root
            && event.attribution_confidence == AttributionConfidence::Definitive
            && event.paths == vec!["src/lib.rs".to_string()]
    }));
    assert!(
        fixture
            .store
            .check_quiescence("root-lease".to_string())
            .await
            .expect("released lease quiescence")
            .quiescent
    );
}

#[tokio::test]
async fn workspace_mutation_finalization_uses_the_authoritative_persisted_lease() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.repo.path().join("owned.txt"), "before\n").expect("owned fixture");
    let lease = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "authoritative-lease-root".to_string(),
                actor_id: "root:authoritative-lease-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec!["owned.txt".to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("mutation lease starts");
    std::fs::write(fixture.repo.path().join("owned.txt"), "after\n").expect("owned edit");
    let mut tampered = lease.clone();
    tampered.actor_id = "forged-actor".to_string();
    tampered.kind = WorkspaceActorKind::Typed;
    tampered.paths = vec![REPOSITORY_WIDE_PATH.to_string()];
    let result = fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), tampered)
        .await
        .expect("persisted lease finalizes");
    assert_eq!(result.changed_paths, vec!["owned.txt".to_string()]);
    let event = fixture
        .store
        .read_workspace_events(fixture.repo.path(), lease.start_epoch)
        .await
        .expect("mutation events")
        .into_iter()
        .find(|event| event.paths == vec!["owned.txt".to_string()])
        .expect("authoritative event");
    assert_eq!(event.actor_id.as_deref(), Some(lease.actor_id.as_str()));
    assert_eq!(event.actor_kind, WorkspaceActorKind::Root);
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
async fn focused_validation_rejects_out_of_scope_repository_mutation_during_execution() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "owned\n").expect("owned fixture");
    std::fs::write(fixture.repo.path().join("outside.txt"), "before\n")
        .expect("out-of-scope fixture");
    let command = "cargo test -p owner validation-integrity";
    let (_, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft("integrity-root", "src/lib.rs", command),
        )
        .await
        .expect("validation assignment");
    let call = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "integrity-proof",
        command,
    )
    .await;
    std::fs::write(fixture.repo.path().join("outside.txt"), "after\n")
        .expect("out-of-scope mutation");
    let call = finish_focused_validation(&fixture.store, call).await;
    assert_eq!(call.status, ValidationCallStatus::Superseded);
    assert!(
        call.evidence
            .stale_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("outside.txt")),
        "the out-of-scope repository mutation is named in the stale reason: {:?}",
        call.evidence.stale_reason
    );
}

#[tokio::test]
async fn stale_recovery_allows_one_reconciliation_then_escalates() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "zero\n").expect("lib fixture");
    let command = "cargo test -p owner targeted";
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft("stale-root", "src/lib.rs", command),
        )
        .await
        .expect("stale assignment");
    finish_focused_validation(
        &fixture.store,
        start_focused_validation(&fixture.store, attempt.attempt_id, "first-proof", command).await,
    )
    .await;
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "one\n").expect("first drift");
    assert!(
        matches!(
            fixture
                .store
                .submit_agent_receipt(
                    attempt.attempt_id,
                    completed_receipt(vec!["first-proof".to_string()])
                )
                .await,
            Err(StoreError::EvidenceSuperseded { .. })
        ),
        "first stale event rejects completion"
    );
    let reconciliation = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "reconciliation-proof",
        command,
    )
    .await;
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "two\n").expect("second drift");
    let reconciliation = finish_focused_validation(&fixture.store, reconciliation).await;
    assert_eq!(
        reconciliation.status,
        ValidationCallStatus::Superseded,
        "the one reconciliation is version checked"
    );
    let task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("escalated task reads");
    assert_eq!(task.current_attempt.state, AttemptState::NeedsMain);
    assert!(
        task.workspace_status
            .next_required_action
            .as_deref()
            .is_some_and(|action| action.contains("isolated workspace")),
        "repeated stale state escalates to root and offers isolation"
    );
    let third = fixture
        .store
        .record_validation_call(ValidationCall {
            call_id: "third-proof".to_string(),
            attempt_id: attempt.attempt_id,
            command_summary: command.to_string(),
            resolved_executable: resolved_test_executable(),
            proof_kind: ValidationProofKind::Focused,
            evidence: ValidationEvidence::default(),
            status: ValidationCallStatus::Running,
            recorded_at: Utc::now(),
        })
        .await;
    assert!(
        matches!(
            third,
            Err(StoreError::AttemptNotActive(_)) | Err(StoreError::StaleRecoveryExhausted(_))
        ),
        "a second stale event cannot start another validation loop"
    );
}

#[tokio::test]
async fn one_workspace_stale_epoch_does_not_double_count_multiple_proofs() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "zero\n").expect("lib fixture");
    let command = "cargo test -p owner multi-proof";
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            validation_worker_draft("multi-proof-root", "src/lib.rs", command),
        )
        .await
        .expect("multi-proof assignment");
    for call_id in ["first-multi-proof", "second-multi-proof"] {
        finish_focused_validation(
            &fixture.store,
            start_focused_validation(&fixture.store, attempt.attempt_id, call_id, command).await,
        )
        .await;
    }
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "one\n").expect("shared drift");
    assert!(
        matches!(
            fixture
                .store
                .submit_agent_receipt(
                    attempt.attempt_id,
                    completed_receipt(vec![
                        "first-multi-proof".to_string(),
                        "second-multi-proof".to_string(),
                    ])
                )
                .await,
            Err(StoreError::EvidenceSuperseded { .. })
        ),
        "the shared stale epoch rejects completion"
    );
    let task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .expect("multi-proof task reads");
    assert_eq!(
        task.current_attempt.state,
        AttemptState::Active,
        "multiple proofs superseded by one workspace epoch consume one recovery event"
    );
    assert_eq!(
        task.workspace_status.next_required_action.as_deref(),
        Some("reconcile stale inputs and run one targeted validation")
    );
    start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "multi-proof-reconciliation",
        command,
    )
    .await;
}

#[tokio::test]
async fn validation_singleflight_is_exact_and_epoch_bound() {
    let fixture = Fixture::new().await;
    let command = "cargo test -p owner identical";
    let mut first_draft = validation_worker_draft("singleflight-root", "unused-a", command);
    first_draft.write_scope.clear();
    let mut second_draft = validation_worker_draft("singleflight-root", "unused-b", command);
    second_draft.write_scope.clear();
    let (_, first_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), first_draft)
        .await
        .expect("first singleflight assignment");
    let (_, second_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), second_draft)
        .await
        .expect("second singleflight assignment");
    let first = start_focused_validation(
        &fixture.store,
        first_attempt.attempt_id,
        "singleflight-leader",
        command,
    )
    .await;
    let second = start_focused_validation(
        &fixture.store,
        second_attempt.attempt_id,
        "singleflight-follower",
        command,
    )
    .await;
    assert_eq!(
        second.evidence.shared_from_call_id.as_deref(),
        Some("singleflight-leader")
    );
    finish_focused_validation(&fixture.store, first).await;
    finish_focused_validation(&fixture.store, second).await;

    fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["epoch-marker".to_string()])
        .await
        .expect("marker baseline");
    std::fs::write(fixture.repo.path().join("epoch-marker"), "changed\n").expect("epoch marker");
    fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["epoch-marker".to_string()])
        .await
        .expect("epoch advances");
    let mut third_draft = validation_worker_draft("singleflight-root", "unused-c", command);
    third_draft.write_scope.clear();
    let (_, third_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), third_draft)
        .await
        .expect("third assignment");
    let third = start_focused_validation(
        &fixture.store,
        third_attempt.attempt_id,
        "new-epoch-validation",
        command,
    )
    .await;
    assert_eq!(third.evidence.shared_from_call_id, None);
}

#[tokio::test]
async fn completed_validation_reuse_is_successful_exact_and_manifest_bound() {
    fn rust_evidence() -> ValidationEvidence {
        ValidationEvidence {
            cwd: Some("codex-rs".to_string()),
            environment_hash: Some("rust-env".to_string()),
            toolchain: Some("stable".to_string()),
            retained_output_ref: Some("tool-call:retained-validation-output".to_string()),
            ..ValidationEvidence::default()
        }
    }

    async fn attempt_for(
        fixture: &Fixture,
        root_session_id: &str,
        scope: &str,
        command: &str,
    ) -> Attempt {
        let draft = validation_worker_draft(root_session_id, scope, command);
        fixture
            .store
            .create_assignment(fixture.repo.path(), draft)
            .await
            .expect("validation assignment")
            .1
    }

    let fixture = Fixture::new().await;
    let rust_source = fixture.repo.path().join("codex-rs/core/src/lib.rs");
    std::fs::create_dir_all(rust_source.parent().expect("rust source parent"))
        .expect("rust source directory");
    std::fs::write(&rust_source, "pub fn source_a() {}\n").expect("rust source fixture");
    let command = "cargo clippy -p codex-core --all-targets";
    let first_attempt = attempt_for(&fixture, "reuse-root", "codex-rs/core", command).await;
    let first = start_focused_validation_with_evidence(
        &fixture.store,
        first_attempt.attempt_id,
        "reuse-success-call",
        command,
        rust_evidence(),
    )
    .await;
    finish_focused_validation(&fixture.store, first).await;
    fixture
        .store
        .submit_agent_receipt(
            first_attempt.attempt_id,
            completed_receipt(vec!["reuse-success-call".to_string()]),
        )
        .await
        .expect("first validation receipt");

    std::fs::write(
        fixture.repo.path().join("validation-report.md"),
        "report only\n",
    )
    .expect("report fixture");
    fixture
        .store
        .capture_workspace_revision(
            fixture.repo.path(),
            vec!["validation-report.md".to_string()],
        )
        .await
        .expect("report revision");
    let report_attempt = attempt_for(&fixture, "reuse-root", "codex-rs/core", command).await;
    let report_follow_up = start_focused_validation_with_evidence(
        &fixture.store,
        report_attempt.attempt_id,
        "reuse-report-call",
        command,
        rust_evidence(),
    )
    .await;
    assert_eq!(
        report_follow_up.evidence.shared_from_call_id.as_deref(),
        Some("reuse-success-call")
    );
    finish_focused_validation(&fixture.store, report_follow_up).await;
    fixture
        .store
        .submit_agent_receipt(
            report_attempt.attempt_id,
            completed_receipt(vec!["reuse-report-call".to_string()]),
        )
        .await
        .expect("report validation receipt");

    std::fs::write(&rust_source, "pub fn source_b() {}\n").expect("changed rust source fixture");
    let changed_attempt = attempt_for(&fixture, "reuse-root", "codex-rs/core", command).await;
    let changed = start_focused_validation_with_evidence(
        &fixture.store,
        changed_attempt.attempt_id,
        "reuse-source-change-call",
        command,
        rust_evidence(),
    )
    .await;
    assert_eq!(changed.evidence.shared_from_call_id, None);
    finish_focused_validation(&fixture.store, changed).await;
    fixture
        .store
        .submit_agent_receipt(
            changed_attempt.attempt_id,
            completed_receipt(vec!["reuse-source-change-call".to_string()]),
        )
        .await
        .expect("changed validation receipt");

    let unknown_attempt = attempt_for(&fixture, "reuse-root", "codex-rs/core", command).await;
    let unknown = start_focused_validation(
        &fixture.store,
        unknown_attempt.attempt_id,
        "reuse-unknown-call",
        command,
    )
    .await;
    assert_eq!(unknown.evidence.shared_from_call_id, None);
    finish_focused_validation(&fixture.store, unknown).await;
    fixture
        .store
        .submit_agent_receipt(
            unknown_attempt.attempt_id,
            completed_receipt(vec!["reuse-unknown-call".to_string()]),
        )
        .await
        .expect("unknown validation receipt");

    let generated_command = "python scripts/check_generated.py";
    let generated_source = fixture.repo.path().join("schema/input.json");
    std::fs::create_dir_all(generated_source.parent().expect("generated source parent"))
        .expect("generated source directory");
    std::fs::write(&generated_source, "{\"version\":\"a\"}\n").expect("generated source fixture");
    let generated_attempt = attempt_for(
        &fixture,
        "generated-root",
        "schema/input.json",
        generated_command,
    )
    .await;
    let generated = start_focused_validation_with_evidence(
        &fixture.store,
        generated_attempt.attempt_id,
        "generated-success-call",
        generated_command,
        ValidationEvidence::default(),
    )
    .await;
    finish_focused_validation(&fixture.store, generated).await;
    fixture
        .store
        .submit_agent_receipt(
            generated_attempt.attempt_id,
            completed_receipt(vec!["generated-success-call".to_string()]),
        )
        .await
        .expect("generated validation receipt");
    std::fs::write(&generated_source, "{\"version\":\"b\"}\n")
        .expect("changed generated source fixture");
    let changed_generated_attempt = attempt_for(
        &fixture,
        "generated-root",
        "schema/input.json",
        generated_command,
    )
    .await;
    let changed_generated = start_focused_validation_with_evidence(
        &fixture.store,
        changed_generated_attempt.attempt_id,
        "generated-change-call",
        generated_command,
        ValidationEvidence::default(),
    )
    .await;
    assert_eq!(changed_generated.evidence.shared_from_call_id, None);
}

#[test]
fn validation_reuse_rejects_every_incomplete_or_legacy_identity() {
    let complete = ValidationEvidence {
        candidate_id: "candidate-a".to_string(),
        implementation_identity: "implementation-a".to_string(),
        source_evidence_epoch: Some(7),
        normalized_invocation: "cargo test -p owner focused".to_string(),
        coverage_identity: "coverage-a".to_string(),
        manifest_hash: "manifest-a".to_string(),
        cwd: Some("codex-rs".to_string()),
        environment_hash: Some("environment-a".to_string()),
        toolchain: Some("stable-a".to_string()),
        features_configuration_identity: "features-a".to_string(),
        covered_input_manifest_hash: "inputs-a".to_string(),
        dependency_manifest_hash: "dependencies-a".to_string(),
        successful_result: Some(true),
        retained_output_digest: "output-a".to_string(),
        retained_output_ref: Some("artifact://output-a".to_string()),
        ..ValidationEvidence::default()
    };
    assert!(complete.has_complete_request_identity());
    assert!(complete.is_reusable_success());

    let mut incomplete = complete.clone();
    incomplete.cwd = None;
    assert!(!incomplete.has_complete_request_identity());
    let mut incomplete = complete.clone();
    incomplete.environment_hash = None;
    assert!(!incomplete.has_complete_request_identity());
    let mut incomplete = complete.clone();
    incomplete.toolchain = None;
    assert!(!incomplete.has_complete_request_identity());
    let mut incomplete = complete.clone();
    incomplete.source_evidence_epoch = None;
    assert!(!incomplete.has_complete_request_identity());
    let mut incomplete = complete.clone();
    incomplete.retained_output_ref = None;
    assert!(incomplete.has_complete_request_identity());
    assert!(!incomplete.is_reusable_success());
    let mut incomplete = complete;
    incomplete.successful_result = None;
    assert!(!incomplete.is_reusable_success());
}

#[tokio::test]
async fn failed_and_cancelled_validation_evidence_is_not_reused() {
    let fixture = Fixture::new().await;
    let command = "cargo clippy -p codex-core --all-targets";
    for (status, suffix) in [
        (ValidationCallStatus::Failed, "failed"),
        (ValidationCallStatus::Cancelled, "cancelled"),
    ] {
        let root_session_id = format!("{suffix}-root");
        let mut first_draft = validation_worker_draft(&root_session_id, "unused-a", command);
        first_draft.write_scope.clear();
        let (_, first_attempt) = fixture
            .store
            .create_assignment(fixture.repo.path(), first_draft)
            .await
            .expect("terminal validation assignment");
        let evidence = ValidationEvidence {
            covered_scopes: vec![RepoScope {
                path: "codex-rs/core".to_string(),
                recursive: true,
            }],
            covered_manifest: vec![WorkspaceManifestEntry {
                path: "codex-rs/core/src/lib.rs".to_string(),
                content_hash: Some("source-a".to_string()),
                existed: true,
            }],
            manifest_hash: "rust-manifest-a".to_string(),
            ..ValidationEvidence::default()
        };
        let mut first = start_focused_validation_with_evidence(
            &fixture.store,
            first_attempt.attempt_id,
            &format!("{suffix}-source"),
            command,
            evidence.clone(),
        )
        .await;
        first.status = status;
        first.recorded_at += Duration::milliseconds(1);
        fixture
            .store
            .record_validation_call(first)
            .await
            .expect("terminal validation finishes");

        let mut retry_draft = validation_worker_draft(&root_session_id, "unused-b", command);
        retry_draft.write_scope.clear();
        let (_, retry_attempt) = fixture
            .store
            .create_assignment(fixture.repo.path(), retry_draft)
            .await
            .expect("retry assignment");
        let retry = start_focused_validation_with_evidence(
            &fixture.store,
            retry_attempt.attempt_id,
            &format!("{suffix}-retry-call"),
            command,
            evidence,
        )
        .await;
        assert_eq!(retry.evidence.shared_from_call_id, None);
        finish_focused_validation(&fixture.store, retry).await;
    }
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
async fn only_new_supporting_evidence_updates_productivity_and_heartbeats_do_not() {
    let fixture = Fixture::new().await;
    std::fs::create_dir_all(fixture.repo.path().join("src")).expect("src directory");
    std::fs::write(fixture.repo.path().join("src/lib.rs"), "evidence\n").expect("evidence fixture");
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("evidence-productivity-root", "src/lib.rs"),
        )
        .await
        .expect("evidence assignment");
    let binding = bind_test_agent(
        &fixture.store,
        assignment.assignment_id,
        attempt.attempt_id,
        "evidence-productivity-root",
    )
    .await;
    let actor_id = format!("attempt:{}", attempt.attempt_id);
    let pool = coordination_pool(&fixture).await;
    let old_progress = fixed_time("2000-01-01T00:00:00Z");
    sqlx::query("UPDATE workspace_actors SET last_progress_at = ? WHERE attempt_id = ?")
        .bind(serde_json::to_string(&old_progress).expect("old progress serializes"))
        .bind(attempt.attempt_id.to_string())
        .execute(&pool)
        .await
        .expect("old progress injects");
    pool.close().await;

    fixture
        .store
        .record_supporting_read(
            fixture.repo.path(),
            actor_id.clone(),
            vec!["src/lib.rs".to_string()],
        )
        .await
        .expect("new evidence persists");
    let pool = coordination_pool(&fixture).await;
    let progressed: String =
        sqlx::query_scalar("SELECT last_progress_at FROM workspace_actors WHERE attempt_id = ?")
            .bind(attempt.attempt_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("progress timestamp reads");
    assert!(
        serde_json::from_str::<chrono::DateTime<Utc>>(&progressed)
            .expect("progress timestamp decodes")
            > old_progress
    );

    sqlx::query("UPDATE workspace_actors SET last_progress_at = ? WHERE attempt_id = ?")
        .bind(serde_json::to_string(&old_progress).expect("old progress serializes"))
        .bind(attempt.attempt_id.to_string())
        .execute(&pool)
        .await
        .expect("old progress resets");
    pool.close().await;
    fixture
        .store
        .record_supporting_read(
            fixture.repo.path(),
            actor_id,
            vec!["src/lib.rs".to_string()],
        )
        .await
        .expect("identical evidence is reusable");
    assert!(
        fixture
            .store
            .heartbeat_typed_workspace_actor(binding)
            .await
            .expect("heartbeat persists")
    );
    let pool = coordination_pool(&fixture).await;
    let unchanged: String =
        sqlx::query_scalar("SELECT last_progress_at FROM workspace_actors WHERE attempt_id = ?")
            .bind(attempt.attempt_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("unchanged progress reads");
    assert_eq!(
        serde_json::from_str::<chrono::DateTime<Utc>>(&unchanged)
            .expect("unchanged progress decodes"),
        old_progress,
        "identical evidence and ordinary heartbeats must not renew productivity"
    );
    pool.close().await;
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
    assert!(call.evidence.lease_expires_at.is_some());
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
    assert!(
        matches!(
            restarted
                .assert_workspace_unclaimed(fixture.repo.path(), None)
                .await,
            Err(StoreError::WorkspaceClaimConflict { .. })
        ),
        "active claims reconstruct across restart"
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
    assert!(
        matches!(
            restarted
                .submit_agent_receipt(
                    attempt.attempt_id,
                    completed_receipt(vec!["restart-validation".to_string()])
                )
                .await,
            Err(StoreError::EvidenceSuperseded { .. })
        ),
        "restart never treats stale proof as current"
    );
    restarted
        .abandon_agent_task(
            TaskActor::Root,
            assignment.assignment_id,
            "root approved terminal cleanup after restart proof".to_string(),
        )
        .await
        .expect("root-approved abandonment seals the linked task");
    let terminal_quiescence = restarted
        .check_quiescence("restart-root".to_string())
        .await
        .expect("terminal quiescence reads");
    assert!(
        terminal_quiescence.quiescent,
        "root completion may proceed once linked assignments, validations, gates, and claims are terminal"
    );
    assert!(terminal_quiescence.active_assignment_ids.is_empty());
    assert!(terminal_quiescence.running_validation_call_ids.is_empty());
    assert!(terminal_quiescence.pending_gate_assignment_ids.is_empty());
    assert!(terminal_quiescence.active_claim_assignment_ids.is_empty());
    assert!(terminal_quiescence.active_mutation_lease_ids.is_empty());
}

#[tokio::test]
async fn workspace_finalization_fence_blocks_writers_and_allows_read_only_quiescence() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.repo.path().join("tracked.txt"), "before\n").expect("workspace fixture");
    let root_session_id = "finalization-root".to_string();
    fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["tracked.txt".to_string()])
        .await
        .expect("initial workspace revision");

    let fence = fixture
        .store
        .begin_workspace_finalization(fixture.repo.path(), root_session_id.clone())
        .await
        .expect("workspace finalization fence");
    let quiescence = fixture
        .store
        .inspect_quiescence(root_session_id.clone())
        .await
        .expect("read-only quiescence under fence");
    assert!(quiescence.quiescent);
    assert!(
        fixture
            .store
            .heartbeat_workspace_finalization(
                fixture.repo.path(),
                fence.fence_id.clone(),
                root_session_id.clone(),
            )
            .await
            .expect("owner heartbeat")
    );
    assert!(
        !fixture
            .store
            .heartbeat_workspace_finalization(
                fixture.repo.path(),
                fence.fence_id.clone(),
                "wrong-root".to_string(),
            )
            .await
            .expect("wrong-owner heartbeat is a clean miss")
    );
    assert!(
        fixture
            .store
            .begin_workspace_finalization(fixture.repo.path(), root_session_id.clone())
            .await
            .is_err(),
        "a second finalization fence must not overlap"
    );
    assert!(
        fixture
            .store
            .capture_workspace_revision(fixture.repo.path(), vec!["tracked.txt".to_string()],)
            .await
            .is_err(),
        "workspace revision writes are fenced"
    );
    assert!(
        fixture
            .store
            .register_workspace_actor(
                fixture.repo.path(),
                WorkspaceActorRegistration {
                    root_session_id: root_session_id.clone(),
                    actor_id: "root-actor".to_string(),
                    kind: WorkspaceActorKind::Root,
                    assignment_id: None,
                    attempt_id: None,
                    strategy: WorkspaceStrategy::Shared,
                },
            )
            .await
            .is_err(),
        "actor registration is fenced"
    );

    fixture
        .store
        .release_workspace_finalization(fixture.repo.path(), fence)
        .await
        .expect("owner releases fence");
    fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["tracked.txt".to_string()])
        .await
        .expect("workspace revision resumes after release");
    fixture
        .store
        .register_workspace_actor(
            fixture.repo.path(),
            WorkspaceActorRegistration {
                root_session_id,
                actor_id: "root-actor".to_string(),
                kind: WorkspaceActorKind::Root,
                assignment_id: None,
                attempt_id: None,
                strategy: WorkspaceStrategy::Shared,
            },
        )
        .await
        .expect("actor registration resumes after release");
}

#[tokio::test]
async fn workspace_finalization_dispatch_seal_blocks_mutations_and_releases() {
    let fixture = Fixture::new().await;
    let root_session_id = "dispatch-seal-root".to_string();
    std::fs::write(fixture.repo.path().join("tracked.txt"), "before\n").expect("workspace fixture");
    fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["tracked.txt".to_string()])
        .await
        .expect("initial workspace revision");
    let fence = fixture
        .store
        .begin_workspace_finalization(fixture.repo.path(), root_session_id.clone())
        .await
        .expect("workspace finalization fence");

    let sealed = fixture
        .store
        .seal_workspace_finalization_dispatch(fixture.repo.path(), fence.clone())
        .await
        .expect("active finalization fence seals for dispatch");
    assert_eq!(sealed.fence_id, fence.fence_id);
    assert!(sealed.expires_at >= fence.expires_at);
    assert_eq!(
        workspace_finalization_fence_state(&fixture, &sealed.fence_id).await,
        "dispatching"
    );
    assert!(
        fixture
            .store
            .heartbeat_workspace_finalization(
                fixture.repo.path(),
                sealed.fence_id.clone(),
                root_session_id.clone(),
            )
            .await
            .expect("dispatching fence heartbeat")
    );
    assert!(
        fixture
            .store
            .begin_workspace_finalization(fixture.repo.path(), root_session_id.clone())
            .await
            .is_err(),
        "dispatching fences remain exclusive"
    );
    let blocked_mutation = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "competing-dispatch-root".to_string(),
                actor_id: "root:competing-dispatch-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await;
    assert!(
        matches!(
            blocked_mutation,
            Err(StoreError::WorkspaceFinalizationActive { .. })
        ),
        "dispatching fence returned the wrong mutation-admission outcome: {blocked_mutation:?}"
    );
    assert!(
        fixture
            .store
            .capture_workspace_revision(fixture.repo.path(), vec!["tracked.txt".to_string()])
            .await
            .is_err(),
        "dispatching fences block trigger-protected workspace writes"
    );

    fixture
        .store
        .release_workspace_finalization(fixture.repo.path(), sealed)
        .await
        .expect("dispatching fence releases");
    fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["tracked.txt".to_string()])
        .await
        .expect("workspace writes resume after dispatching release");
}

#[tokio::test]
async fn workspace_finalization_dispatch_seal_rejects_wrong_or_expired_identity() {
    let fixture = Fixture::new().await;
    let fence = fixture
        .store
        .begin_workspace_finalization(fixture.repo.path(), "dispatch-owner".to_string())
        .await
        .expect("workspace finalization fence");
    let mut wrong_owner = fence.clone();
    wrong_owner.root_session_id = "wrong-dispatch-owner".to_string();
    assert!(matches!(
        fixture
            .store
            .seal_workspace_finalization_dispatch(fixture.repo.path(), wrong_owner)
            .await,
        Err(StoreError::WorkspaceLeaseUnavailable(_))
    ));

    expire_workspace_finalization_fence(&fixture, &fence.fence_id).await;
    assert!(
        !fixture
            .store
            .heartbeat_workspace_finalization(
                fixture.repo.path(),
                fence.fence_id.clone(),
                fence.root_session_id.clone(),
            )
            .await
            .expect("expired finalization heartbeat is a clean miss")
    );
    assert!(matches!(
        fixture
            .store
            .seal_workspace_finalization_dispatch(fixture.repo.path(), fence)
            .await,
        Err(StoreError::WorkspaceLeaseUnavailable(_))
    ));
}

#[tokio::test]
async fn expired_dispatching_finalization_fence_recovers_after_crash() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.repo.path().join("tracked.txt"), "before\n").expect("workspace fixture");
    fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["tracked.txt".to_string()])
        .await
        .expect("initial workspace revision");
    let fence = fixture
        .store
        .begin_workspace_finalization(fixture.repo.path(), "crashed-dispatch-root".to_string())
        .await
        .expect("workspace finalization fence");
    let sealed = fixture
        .store
        .seal_workspace_finalization_dispatch(fixture.repo.path(), fence)
        .await
        .expect("finalization fence seals for dispatch");
    expire_workspace_finalization_fence(&fixture, &sealed.fence_id).await;

    let recovered_lease = fixture
        .store
        .begin_workspace_mutation(
            fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "recovered-mutation-root".to_string(),
                actor_id: "root:recovered-mutation-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("expired dispatching fence does not strand mutation admission");
    fixture
        .store
        .finish_workspace_mutation(fixture.repo.path(), recovered_lease)
        .await
        .expect("recovered workspace mutation finishes");

    let replacement = fixture
        .store
        .begin_workspace_finalization(fixture.repo.path(), "replacement-root".to_string())
        .await
        .expect("expired dispatching fence does not strand finalization");
    assert_eq!(
        workspace_finalization_fence_state(&fixture, &sealed.fence_id).await,
        "expired"
    );
    fixture
        .store
        .release_workspace_finalization(fixture.repo.path(), replacement)
        .await
        .expect("replacement finalization releases");
    fixture
        .store
        .capture_workspace_revision(fixture.repo.path(), vec!["tracked.txt".to_string()])
        .await
        .expect("workspace writes resume after simulated dispatch crash");
}

#[tokio::test]
async fn expired_workspace_finalization_fence_does_not_strand_task_writes() {
    let fixture = Fixture::new().await;
    let root_session_id = "expired-finalization-root";
    let mut draft = worker_draft(root_session_id, "first");
    draft.required_evidence = vec!["focused test".to_string()];
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), draft)
        .await
        .expect("worker assignment");
    let fence = fixture
        .store
        .begin_workspace_finalization(fixture.repo.path(), root_session_id.to_string())
        .await
        .expect("workspace finalization fence");

    let binding_error = fixture
        .store
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: assignment.assignment_id,
            attempt_id: attempt.attempt_id,
            agent_path: "/root/expired-finalization".to_string(),
            task_name: "expired-finalization".to_string(),
            thread_id: Some("expired-finalization-thread".to_string()),
        })
        .await
        .expect_err("live finalization fence blocks bindings");
    assert!(
        binding_error
            .to_string()
            .contains("workspace finalization active")
    );
    let gate_error = fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            assignment.assignment_id,
            GateKind::Review,
            GateStatus::Pending,
            "review required".to_string(),
        )
        .await
        .expect_err("live finalization fence blocks gates");
    assert!(
        gate_error
            .to_string()
            .contains("workspace finalization active")
    );
    let validation_error = fixture
        .store
        .record_validation_call(ValidationCall {
            call_id: "expired-finalization-validation".to_string(),
            attempt_id: attempt.attempt_id,
            command_summary: "focused test".to_string(),
            resolved_executable: resolved_test_executable(),
            proof_kind: ValidationProofKind::Focused,
            evidence: ValidationEvidence::default(),
            status: ValidationCallStatus::Running,
            recorded_at: Utc::now(),
        })
        .await
        .expect_err("live finalization fence blocks validation writes");
    assert!(
        validation_error
            .to_string()
            .contains("workspace finalization active")
    );

    expire_workspace_finalization_fence(&fixture, &fence.fence_id).await;

    bind_test_agent(
        &fixture.store,
        assignment.assignment_id,
        attempt.attempt_id,
        root_session_id,
    )
    .await;
    fixture
        .store
        .set_agent_gate(
            TaskActor::Root,
            assignment.assignment_id,
            GateKind::Review,
            GateStatus::Pending,
            "review required".to_string(),
        )
        .await
        .expect("expired fence allows gate writes");
    let validation = start_focused_validation(
        &fixture.store,
        attempt.attempt_id,
        "expired-finalization-validation",
        "focused test",
    )
    .await;
    finish_focused_validation(&fixture.store, validation).await;
    fixture
        .store
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt(vec!["expired-finalization-validation".to_string()]),
        )
        .await
        .expect("expired fence allows receipt and attempt writes");
    fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft(root_session_id, "second"))
        .await
        .expect("expired fence allows new attempt writes");
    let replacement = fixture
        .store
        .begin_workspace_finalization(fixture.repo.path(), "replacement-active-root".to_string())
        .await
        .expect("expired active fence does not strand finalization");
    fixture
        .store
        .release_workspace_finalization(fixture.repo.path(), replacement)
        .await
        .expect("replacement active finalization releases");
}

#[tokio::test]
async fn json_timestamps_order_validation_calls_and_bindings_by_instant() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(
            fixture.repo.path(),
            worker_draft("timestamp-order-root", "validation"),
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
                resolved_executable: None,
                proof_kind: ValidationProofKind::LegacyUnclassified,
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
    let mut first_draft = worker_draft("timestamp-singleflight-root", "singleflight/first");
    first_draft.required_evidence = vec!["focused test".to_string()];
    let (_, first_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), first_draft)
        .await
        .expect("first singleflight assignment");
    let mut second_draft = worker_draft("timestamp-singleflight-root", "singleflight/second");
    second_draft.required_evidence = vec!["focused test".to_string()];
    let (second_assignment, second_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), second_draft)
        .await
        .expect("second singleflight assignment");
    let comparison_now = fixed_time("2099-01-01T00:00:00.001Z");
    crate::local::with_test_comparison_now(
        comparison_now,
        fixture.store.record_validation_call(ValidationCall {
            call_id: "fraction-leader".to_string(),
            attempt_id: first_attempt.attempt_id,
            command_summary: "focused test".to_string(),
            resolved_executable: resolved_test_executable(),
            proof_kind: ValidationProofKind::Focused,
            evidence: ValidationEvidence {
                lease_expires_at: Some(fixed_time("2099-01-01T00:00:00Z")),
                ..ValidationEvidence::default()
            },
            status: ValidationCallStatus::Running,
            recorded_at: comparison_now,
        }),
    )
    .await
    .expect("zero-width leader starts");
    crate::local::with_test_comparison_now(
        comparison_now,
        fixture.store.record_validation_call(ValidationCall {
            call_id: "fraction-successor".to_string(),
            attempt_id: second_attempt.attempt_id,
            command_summary: "focused test".to_string(),
            resolved_executable: resolved_test_executable(),
            proof_kind: ValidationProofKind::Focused,
            evidence: ValidationEvidence::default(),
            status: ValidationCallStatus::Running,
            recorded_at: comparison_now,
        }),
    )
    .await
    .expect("expired leader is replaced");
    let successor = fixture
        .store
        .get_agent_task(second_assignment.assignment_id, Some(0))
        .await
        .expect("successor task reads")
        .validation_calls
        .into_iter()
        .find(|call| call.call_id == "fraction-successor")
        .expect("successor validation call exists");
    assert_eq!(successor.evidence.shared_from_call_id, None);

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
async fn json_timestamp_comparisons_preserve_lease_liveness_and_fence_behavior() {
    let lease_fixture = Fixture::new().await;
    std::fs::write(lease_fixture.repo.path().join("tracked.txt"), "before\n")
        .expect("lease fixture file");
    let lease = lease_fixture
        .store
        .begin_workspace_mutation(
            lease_fixture.repo.path(),
            WorkspaceMutationRequest {
                root_session_id: "timestamp-lease-root".to_string(),
                actor_id: "root:timestamp-lease-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec!["tracked.txt".to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
        )
        .await
        .expect("timestamp lease starts");
    let pool = coordination_pool(&lease_fixture).await;
    sqlx::query("UPDATE workspace_mutation_leases SET expires_at = ? WHERE lease_id = ?")
        .bind(json_time("2099-01-01T00:00:00.001Z"))
        .bind(&lease.lease_id)
        .execute(&pool)
        .await
        .expect("three-digit lease expiration updates");
    pool.close().await;
    let lease_now = fixed_time("2099-01-01T00:00:00Z");
    let claim_result = crate::local::with_test_comparison_now(
        lease_now,
        lease_fixture.store.create_assignment(
            lease_fixture.repo.path(),
            worker_draft("timestamp-lease-root", "tracked.txt"),
        ),
    )
    .await;
    assert!(
        claim_result.is_err(),
        "live mutation lease must block claims"
    );
    let quiescence = crate::workspace::with_test_comparison_now(
        lease_now,
        lease_fixture
            .store
            .inspect_quiescence("timestamp-lease-root".to_string()),
    )
    .await
    .expect("live mixed-width lease remains visible");
    assert_eq!(
        quiescence.active_mutation_lease_ids,
        vec![lease.lease_id.clone()]
    );
    assert!(matches!(
        crate::workspace::with_test_comparison_now(
            lease_now,
            lease_fixture.store.begin_workspace_finalization(
                lease_fixture.repo.path(),
                "timestamp-finalization-root".to_string(),
            ),
        )
        .await,
        Err(StoreError::WorkspaceFinalizationNotQuiescent { .. })
    ));

    let liveness_fixture = Fixture::new().await;
    let (owner, owner_attempt) = liveness_fixture
        .store
        .create_assignment(
            liveness_fixture.repo.path(),
            worker_draft("timestamp-owner-root", "owned"),
        )
        .await
        .expect("timestamp owner assignment");
    let pool = coordination_pool(&liveness_fixture).await;
    sqlx::query(
        "UPDATE workspace_actors
         SET state = 'active', lease_expires_at = ?, last_progress_at = ?
         WHERE attempt_id = ?",
    )
    .bind(json_time("2099-01-01T00:00:00Z"))
    .bind(json_time("2098-01-01T00:00:00Z"))
    .bind(owner_attempt.attempt_id.to_string())
    .execute(&pool)
    .await
    .expect("owner liveness timestamps update");
    pool.close().await;
    let liveness_now = fixed_time("2099-01-01T00:00:00.001Z");
    crate::local::with_test_comparison_now(
        liveness_now,
        liveness_fixture.store.create_assignment(
            liveness_fixture.repo.path(),
            worker_draft("timestamp-replacement-root", "owned"),
        ),
    )
    .await
    .expect("chronologically expired owner releases its claim");
    assert_eq!(
        liveness_fixture
            .store
            .get_agent_task(owner.assignment_id, Some(0))
            .await
            .expect("expired owner task reads")
            .current_attempt
            .state,
        AttemptState::NeedsMain
    );

    let fence_fixture = Fixture::new().await;
    let fence = fence_fixture
        .store
        .begin_workspace_finalization(
            fence_fixture.repo.path(),
            "timestamp-fence-owner".to_string(),
        )
        .await
        .expect("timestamp fence starts");
    let pool = coordination_pool(&fence_fixture).await;
    sqlx::query("UPDATE workspace_finalization_fences SET expires_at = ? WHERE fence_id = ?")
        .bind(json_time("2099-01-01T00:00:00Z"))
        .bind(&fence.fence_id)
        .execute(&pool)
        .await
        .expect("zero-width fence expiration updates");
    pool.close().await;
    let replacement = crate::workspace::with_test_comparison_now(
        fixed_time("2099-01-01T00:00:00.001Z"),
        fence_fixture.store.begin_workspace_finalization(
            fence_fixture.repo.path(),
            "timestamp-fence-replacement".to_string(),
        ),
    )
    .await
    .expect("chronologically expired fence permits replacement");
    fence_fixture
        .store
        .release_workspace_finalization(fence_fixture.repo.path(), replacement)
        .await
        .expect("replacement fence releases");
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
        .expect("upgraded assignment may mutate its claimed workspace");
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
    let (integrator, integrator_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), integrator_draft)
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
    controlled_write(
        &fixture.store,
        fixture.repo.path(),
        "isolation-root",
        integrator.assignment_id,
        integrator_attempt.attempt_id,
        "src/lib.rs",
        "isolated implementation\n",
    )
    .await;
    finish_focused_validation(
        &fixture.store,
        start_focused_validation(
            &fixture.store,
            integrator_attempt.attempt_id,
            "integrator-validation",
            integrator_command,
        )
        .await,
    )
    .await;
    fixture
        .store
        .submit_agent_receipt(
            integrator_attempt.attempt_id,
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
