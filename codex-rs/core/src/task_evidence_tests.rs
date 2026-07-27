use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

async fn ledger_fixture() -> (tempfile::TempDir, PathBuf, TaskEvidenceLedger) {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let codex_home = temp.path().join("home");
    tokio::fs::create_dir_all(repo.join("scripts"))
        .await
        .expect("scripts");
    tokio::fs::create_dir_all(repo.join(".git"))
        .await
        .expect("git dir");
    tokio::fs::write(repo.join("scripts/verify_local.py"), "# fixture")
        .await
        .expect("verifier");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("manifest");
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("absolute repo");
    let ledger = TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), cwd.as_path()).await;
    (temp, repo, ledger)
}

#[cfg(unix)]
fn create_directory_alias(target: &Path, alias: &Path) {
    std::os::unix::fs::symlink(target, alias).expect("create directory symlink");
}

#[cfg(windows)]
fn create_directory_alias(target: &Path, alias: &Path) {
    let output = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(alias)
        .arg(target)
        .output()
        .expect("create directory junction");
    assert!(
        output.status.success(),
        "mklink /J failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn existing_evidence_reuses_canonical_repository_identity() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let repo_alias = temp.path().join("repo-alias");
    let codex_home = temp.path().join("home");
    tokio::fs::create_dir_all(repo.join("scripts"))
        .await
        .expect("scripts");
    tokio::fs::create_dir_all(repo.join(".git"))
        .await
        .expect("git dir");
    tokio::fs::write(repo.join("scripts/verify_local.py"), "# fixture")
        .await
        .expect("verifier");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("manifest");
    create_directory_alias(&repo, &repo_alias);

    let thread_id = ThreadId::new();
    let evidence_path = codex_home
        .join("task-evidence")
        .join(format!("{thread_id}.json"));
    let ledger = TaskEvidenceLedger::load_or_new(codex_home.clone(), thread_id, &repo).await;
    drop(ledger);

    let mut value = serde_json::from_slice::<Value>(
        &tokio::fs::read(&evidence_path)
            .await
            .expect("persisted evidence"),
    )
    .expect("valid evidence");
    value["start"]["repository_root"] = Value::String(repo_alias.to_string_lossy().into_owned());
    tokio::fs::write(
        &evidence_path,
        serde_json::to_vec_pretty(&value).expect("serialize evidence"),
    )
    .await
    .expect("rewrite legacy repository root");

    let reloaded = TaskEvidenceLedger::load_or_new(codex_home.clone(), thread_id, &repo).await;
    let guard = reloaded.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.revision, 2);
    assert_eq!(
        PathBuf::from(&document.start.repository_root),
        canonical_repository_root(&repo)
    );
    drop(guard);

    let mut entries = tokio::fs::read_dir(codex_home.join("task-evidence"))
        .await
        .expect("evidence directory");
    while let Some(entry) = entries.next_entry().await.expect("evidence entry") {
        assert!(
            !entry.file_name().to_string_lossy().ends_with(".preserved"),
            "matching repository evidence must not be quarantined"
        );
    }
}

#[cfg(windows)]
#[test]
fn repository_identity_canonicalizes_drive_letter_case() {
    let temp = tempfile::tempdir().expect("tempdir");
    let canonical = canonical_repository_root(temp.path());
    let mut alternate = canonical.to_string_lossy().into_owned();
    let drive = alternate.as_bytes().first().copied().expect("drive letter");
    assert_eq!(alternate.as_bytes().get(1), Some(&b':'));
    let alternate_drive = if drive.is_ascii_uppercase() {
        drive.to_ascii_lowercase()
    } else {
        drive.to_ascii_uppercase()
    };
    alternate.replace_range(0..1, &(alternate_drive as char).to_string());

    assert!(repository_root_paths_equal(
        &canonical,
        Path::new(&alternate)
    ));
    assert!(repository_roots_match(&canonical, Path::new(&alternate)));
}

#[tokio::test]
async fn verifier_repo_root_must_match_the_task_evidence_root() {
    let (temp, repo, ledger) = ledger_fixture().await;
    let other_repo = temp.path().join("other-repo");
    tokio::fs::create_dir_all(&other_repo)
        .await
        .expect("other repo");

    assert!(ledger.matches_repo_root(&repo));
    assert!(ledger.matches_repo_root(&repo.join(".")));
    assert!(!ledger.matches_repo_root(&other_repo));
    assert!(!TaskEvidenceLedger::disabled().matches_repo_root(&repo));
}

async fn initialize_git_repo(repo: &Path) {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["init", "--quiet"])
        .output()
        .await
        .expect("git init should run");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn plan_item(id: &str, status: StepStatus) -> PlanItemArg {
    PlanItemArg {
        id: Some(id.to_string()),
        step: format!("Implement {id}"),
        status,
        depends_on: Vec::new(),
        acceptance_criteria: vec!["focused validation passes".to_string()],
        runtime_paths: vec![format!("src/{id}.rs")],
        generated_artifacts: Vec::new(),
        risks: Vec::new(),
        requires_desktop_activation: false,
    }
}

fn plan_with(items: Vec<PlanItemArg>) -> UpdatePlanArgs {
    UpdatePlanArgs {
        explanation: None,
        plan: items,
    }
}

fn command_receipt(id: &str) -> CommandReceipt {
    CommandReceipt {
        id: id.to_string(),
        recorded_at: timestamp(),
        epoch: 0,
        step_id: None,
        command: vec!["true".to_string()],
        cwd: ".".to_string(),
        exit_code: 0,
        timed_out: false,
        duration_ms: 1,
        possible_mutation: false,
    }
}

fn validation_receipt(id: &str) -> ValidationReceipt {
    ValidationReceipt {
        id: id.to_string(),
        recorded_at: timestamp(),
        epoch: 0,
        step_id: Some("step".to_string()),
        mode: "final".to_string(),
        verdict: Some("VERIFIED".to_string()),
        tool_success: true,
        proof_bearing: true,
        active_files: Vec::new(),
        stale_reasons: Vec::new(),
        payload: None,
    }
}

#[tokio::test]
async fn multiple_in_progress_steps_are_preserved_and_block_completion() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let normalized = ledger
        .record_plan_update(&plan_with(vec![
            plan_item("one", StepStatus::InProgress),
            plan_item("two", StepStatus::InProgress),
        ]))
        .await;

    assert_eq!(normalized.plan[0].status, StepStatus::InProgress);
    assert_eq!(normalized.plan[1].status, StepStatus::InProgress);
    let gate = ledger.completion_gate().await.expect("gate");
    assert_eq!(gate.status, TaskCompletionStatus::Blocked);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("multiple in-progress steps"))
    );
}

#[tokio::test]
async fn duplicate_explicit_step_ids_are_renamed_and_block_completion() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let normalized = ledger
        .record_plan_update(&plan_with(vec![
            plan_item("duplicate", StepStatus::Pending),
            plan_item("duplicate", StepStatus::Pending),
        ]))
        .await;

    assert_ne!(normalized.plan[0].id, normalized.plan[1].id);
    let gate = ledger.completion_gate().await.expect("gate");
    assert_eq!(gate.status, TaskCompletionStatus::Blocked);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("duplicate explicit step ids"))
    );
}

#[tokio::test]
async fn failed_edit_does_not_promote_the_active_step() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("src");
    tokio::fs::write(repo.join("src/step.rs"), "pub fn value() -> u8 { 1 }")
        .await
        .expect("source");
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::InProgress)]))
        .await;
    ledger
        .record_edit_intent("failed-edit", &repo, &[PathBuf::from("src/step.rs")])
        .await;
    tokio::fs::write(repo.join("src/step.rs"), "pub fn value() -> u8 { 2 }")
        .await
        .expect("source update");
    ledger.record_edit_result("failed-edit", "failed").await;

    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.plan[0].status, StepStatus::InProgress);
    assert_eq!(document.edit_receipts[0].outcome, "failed");
}

#[tokio::test]
async fn failed_mutating_command_does_not_promote_the_active_step() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::InProgress)]))
        .await;
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo");
    ledger
        .record_command(
            &["touch".to_string(), "src/step.rs".to_string()],
            &PathUri::from_abs_path(&cwd),
            1,
            false,
            1,
            true,
        )
        .await;

    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.plan[0].status, StepStatus::InProgress);
}

#[test]
fn verifier_requirements_require_an_exact_successful_result() {
    let requirement = GeneratedArtifactRequirement {
        id: "surface:config:validate".to_string(),
        step_id: Some("step".to_string()),
        path: None,
        validation_command: vec!["just".to_string(), "config-schema-check".to_string()],
        source: "verify_local".to_string(),
        validation_receipt_ids: Vec::new(),
    };
    let matching = serde_json::json!({
        "results": [{
            "id": "surface:config:validate",
            "command": ["just", "config-schema-check"],
            "status": "VERIFIED",
            "exit_code": 0,
            "timed_out": false
        }]
    });
    assert!(verifier_requirement_satisfied(
        &requirement,
        Some(&matching)
    ));

    let wrong_command = serde_json::json!({
        "results": [{
            "id": "surface:config:validate",
            "command": ["just", "different-check"],
            "status": "VERIFIED",
            "exit_code": 0,
            "timed_out": false
        }]
    });
    assert!(!verifier_requirement_satisfied(
        &requirement,
        Some(&wrong_command)
    ));
}

#[tokio::test]
async fn edit_free_step_passes_with_fresh_validation_evidence() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut audit = plan_item("audit", StepStatus::Implemented);
    audit.step = "Audit the runtime behavior".to_string();
    ledger.record_plan_update(&plan_with(vec![audit])).await;

    let plan_validation_start = ledger
        .begin_verify_local_validation(&[])
        .await
        .expect("plan validation start");
    ledger
        .record_verify_local(
            "plan",
            Some("PLANNED"),
            true,
            false,
            Some(&plan_validation_start),
            &[],
            &[],
            Some(&serde_json::json!({"planned": []})),
        )
        .await;
    let final_validation_start = ledger
        .begin_verify_local_validation(&[])
        .await
        .expect("final validation start");
    assert!(
        ledger
            .record_verify_local(
                "final",
                Some("VERIFIED"),
                true,
                true,
                Some(&final_validation_start),
                &[],
                &[],
                Some(&serde_json::json!({"verdict": "VERIFIED"})),
            )
            .await
    );

    {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert!(document.plan[0].edit_paths.is_empty());
        assert_eq!(document.plan[0].validation_receipt_ids.len(), 1);
        assert_eq!(document.plan[0].status, StepStatus::Passed);
    }
    assert_eq!(
        ledger.completion_gate().await.expect("gate").status,
        TaskCompletionStatus::Passed
    );
}

#[tokio::test]
async fn pending_edit_free_step_does_not_capture_early_validation_evidence() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item(
            "future-audit",
            StepStatus::Pending,
        )]))
        .await;

    let plan_validation_start = ledger
        .begin_verify_local_validation(&[])
        .await
        .expect("plan validation start");
    ledger
        .record_verify_local(
            "plan",
            Some("PLANNED"),
            true,
            false,
            Some(&plan_validation_start),
            &[],
            &[],
            Some(&serde_json::json!({"planned": []})),
        )
        .await;
    let final_validation_start = ledger
        .begin_verify_local_validation(&[])
        .await
        .expect("final validation start");
    assert!(
        ledger
            .record_verify_local(
                "final",
                Some("VERIFIED"),
                true,
                true,
                Some(&final_validation_start),
                &[],
                &[],
                Some(&serde_json::json!({"verdict": "VERIFIED"})),
            )
            .await
    );

    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert!(document.plan[0].edit_paths.is_empty());
    assert!(document.plan[0].validation_receipt_ids.is_empty());
    assert_eq!(document.plan[0].status, StepStatus::Pending);
}

#[tokio::test]
async fn generated_artifact_mutation_invalidates_validation_freshness() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("generated"))
        .await
        .expect("generated directory");
    tokio::fs::write(repo.join("generated/schema.json"), br#"{"version":1}"#)
        .await
        .expect("generated artifact");

    let mut item = plan_item("step", StepStatus::Implemented);
    item.generated_artifacts = vec!["generated/schema.json".to_string()];
    ledger.record_plan_update(&plan_with(vec![item])).await;
    let validation_start = ledger
        .begin_verify_local_validation(&[])
        .await
        .expect("validation start");
    ledger
        .record_verify_local(
            "final",
            Some("VERIFIED"),
            true,
            true,
            Some(&validation_start),
            &[],
            &[],
            Some(&serde_json::json!({"verdict": "VERIFIED"})),
        )
        .await;

    {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert!(generated_artifact_is_fresh(
            document,
            "generated/schema.json"
        ));
    }

    tokio::fs::write(repo.join("generated/schema.json"), br#"{"version":2}"#)
        .await
        .expect("mutated generated artifact");
    ledger.refresh_external_file_freshness().await;

    {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        assert!(!generated_artifact_is_fresh(
            document,
            "generated/schema.json"
        ));
        assert!(document.risks.iter().any(|risk| {
            risk.id == generated_artifact_freshness_risk_id("generated/schema.json")
                && risk.blocking
                && !risk.resolved
        }));
    }

    let revalidation_start = ledger
        .begin_verify_local_validation(&[])
        .await
        .expect("revalidation start");
    ledger
        .record_verify_local(
            "final",
            Some("VERIFIED"),
            true,
            true,
            Some(&revalidation_start),
            &[],
            &[],
            Some(&serde_json::json!({"verdict": "VERIFIED"})),
        )
        .await;
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert!(generated_artifact_is_fresh(
        document,
        "generated/schema.json"
    ));
    assert!(
        document
            .risks
            .iter()
            .filter(|risk| {
                matches!(
                    risk.source.as_str(),
                    "freshness" | "generated_artifact_freshness"
                )
            })
            .all(|risk| risk.resolved)
    );
}

#[tokio::test]
async fn migration_repairs_duplicate_receipts_and_invalidates_ambiguous_links() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    document.plan = vec![EvidencePlanStep {
        id: "step".to_string(),
        step: "step".to_string(),
        status: StepStatus::Passed,
        depends_on: Vec::new(),
        acceptance_criteria: Vec::new(),
        runtime_paths: Vec::new(),
        generated_artifacts: Vec::new(),
        risks: Vec::new(),
        requires_desktop_activation: false,
        edit_paths: BTreeSet::from(["src/step.rs".to_string()]),
        validation_receipt_ids: vec!["validation-1".to_string()],
    }];
    document.command_receipts = vec![command_receipt("command-1"), command_receipt("command-1")];
    document.validation_receipts = vec![
        validation_receipt("validation-1"),
        validation_receipt("validation-1"),
    ];
    migrate_document(&mut document);

    assert_ne!(
        document.command_receipts[0].id,
        document.command_receipts[1].id
    );
    assert_ne!(
        document.validation_receipts[0].id,
        document.validation_receipts[1].id
    );
    assert!(document.plan[0].validation_receipt_ids.is_empty());
    assert_eq!(document.plan[0].status, StepStatus::Implemented);
}

#[tokio::test]
async fn migration_drops_unattributed_legacy_file_hashes() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    document.plan = vec![EvidencePlanStep {
        id: "step".to_string(),
        step: "step".to_string(),
        status: StepStatus::Implemented,
        depends_on: Vec::new(),
        acceptance_criteria: Vec::new(),
        runtime_paths: Vec::new(),
        generated_artifacts: Vec::new(),
        risks: Vec::new(),
        requires_desktop_activation: false,
        edit_paths: BTreeSet::from(["src/owned.rs".to_string()]),
        validation_receipt_ids: Vec::new(),
    }];
    document.latest_file_hashes = BTreeMap::from([
        (
            "src/owned.rs".to_string(),
            FileHashSnapshot {
                path: "src/owned.rs".to_string(),
                sha1: Some("a".repeat(40)),
                exists: true,
                read_error: None,
            },
        ),
        (
            "src/unrelated.rs".to_string(),
            FileHashSnapshot {
                path: "src/unrelated.rs".to_string(),
                sha1: Some("b".repeat(40)),
                exists: true,
                read_error: None,
            },
        ),
    ]);

    migrate_document(&mut document);

    assert_eq!(
        document
            .latest_file_hashes
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["src/owned.rs".to_string()]
    );
}

#[tokio::test]
async fn dangling_validation_receipt_cannot_leave_a_step_passed() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    document.verify_plan_epoch = Some(document.evidence_epoch);
    document.validation_epoch = Some(document.evidence_epoch);
    document.plan = vec![EvidencePlanStep {
        id: "step".to_string(),
        step: "step".to_string(),
        status: StepStatus::Passed,
        depends_on: Vec::new(),
        acceptance_criteria: Vec::new(),
        runtime_paths: Vec::new(),
        generated_artifacts: Vec::new(),
        risks: Vec::new(),
        requires_desktop_activation: false,
        edit_paths: BTreeSet::from(["src/step.txt".to_string()]),
        validation_receipt_ids: vec!["validation-1".to_string()],
    }];
    let mut proof = validation_receipt("validation-1");
    proof.active_files = vec![FileHashSnapshot {
        path: "src/step.txt".to_string(),
        sha1: Some("a".repeat(40)),
        exists: true,
        read_error: None,
    }];
    document.validation_receipts = vec![proof];
    for sequence in 2..=MAX_VALIDATION_RECEIPTS + 1 {
        let mut receipt = validation_receipt(&format!("validation-{sequence}"));
        receipt.proof_bearing = false;
        document.validation_receipts.push(receipt);
    }
    trim_to_last(&mut document.validation_receipts, MAX_VALIDATION_RECEIPTS);
    assert!(
        document
            .validation_receipts
            .iter()
            .all(|receipt| receipt.id != "validation-1")
    );

    assert_eq!(
        derive_completion_gate(&document, None).status,
        TaskCompletionStatus::Partial
    );
    promote_steps_with_fresh_evidence(&mut document);
    assert_eq!(document.plan[0].status, StepStatus::Implemented);
}

#[tokio::test]
async fn storage_failure_is_tracked_and_fail_closed() {
    let (_temp, _repo, mut ledger) = ledger_fixture().await;
    ledger.evidence_path = None;
    {
        let mut guard = ledger.document.lock().await;
        let document = guard.as_mut().expect("document");
        let epoch = document.evidence_epoch;
        upsert_risk(
            document,
            task_evidence_storage_risk("quarantine failed", epoch),
        );
    }

    let gate = ledger.completion_gate().await.expect("fail-closed gate");
    assert_eq!(gate.status, TaskCompletionStatus::Blocked);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("storage is unavailable"))
    );
}

#[tokio::test]
async fn validation_rejects_files_that_change_after_the_start_snapshot() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("src");
    tokio::fs::write(repo.join("src/step.rs"), "pub fn value() -> u8 { 1 }")
        .await
        .expect("source");
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::InProgress)]))
        .await;
    ledger
        .record_edit_intent("edit", &repo, &[PathBuf::from("src/step.rs")])
        .await;
    tokio::fs::write(repo.join("src/step.rs"), "pub fn value() -> u8 { 2 }")
        .await
        .expect("edited source");
    ledger.record_edit_result("edit", "completed").await;
    let validation_start = ledger
        .begin_verify_local_validation(&[])
        .await
        .expect("validation start");
    tokio::fs::write(repo.join("src/step.rs"), "pub fn value() -> u8 { 3 }")
        .await
        .expect("concurrent source update");
    let proof_accepted = ledger
        .record_verify_local(
            "final",
            Some("VERIFIED"),
            true,
            true,
            Some(&validation_start),
            &[PathBuf::from("src/step.rs")],
            &[],
            Some(&serde_json::json!({"verdict": "VERIFIED"})),
        )
        .await;

    assert!(!proof_accepted);
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.validation_epoch, None);
    assert_ne!(document.plan[0].status, StepStatus::Passed);
    assert!(
        document
            .risks
            .iter()
            .any(|risk| { risk.id == "verify-local-concurrent-change" && !risk.resolved })
    );
}

#[tokio::test]
async fn validation_checks_explicit_requested_paths_when_reported_scope_is_empty() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::write(repo.join("requested.rs"), "pub fn value() -> u8 { 1 }")
        .await
        .expect("requested source");
    let validation_start = ledger
        .begin_verify_local_validation(&[PathBuf::from("requested.rs")])
        .await
        .expect("validation start");
    assert!(validation_start.owned_file_paths.contains("requested.rs"));
    tokio::fs::write(repo.join("requested.rs"), "pub fn value() -> u8 { 2 }")
        .await
        .expect("requested source update");

    let proof_accepted = ledger
        .record_verify_local(
            "final",
            Some("VERIFIED"),
            true,
            true,
            Some(&validation_start),
            &[],
            &[],
            Some(&serde_json::json!({"verdict": "VERIFIED"})),
        )
        .await;

    assert!(!proof_accepted);
}

#[tokio::test]
async fn validation_ignores_unrelated_dirty_file_changes() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("src");
    tokio::fs::write(repo.join("src/step.rs"), "pub fn value() -> u8 { 1 }")
        .await
        .expect("source");
    tokio::fs::write(repo.join("src/unrelated.rs"), "pub fn value() -> u8 { 1 }")
        .await
        .expect("unrelated source");
    ledger
        .record_plan_update(&plan_with(vec![plan_item("step", StepStatus::InProgress)]))
        .await;
    ledger
        .record_edit_intent("edit", &repo, &[PathBuf::from("src/step.rs")])
        .await;
    tokio::fs::write(repo.join("src/step.rs"), "pub fn value() -> u8 { 2 }")
        .await
        .expect("edited source");
    ledger.record_edit_result("edit", "completed").await;

    let validation_start = ledger
        .begin_verify_local_validation(&[])
        .await
        .expect("validation start");
    assert!(validation_start.file_snapshots.contains_key("src/step.rs"));
    assert!(
        validation_start
            .file_snapshots
            .contains_key("src/unrelated.rs")
    );
    assert!(validation_start.owned_file_paths.contains("src/step.rs"));
    assert!(
        !validation_start
            .owned_file_paths
            .contains("src/unrelated.rs")
    );
    tokio::fs::write(repo.join("src/unrelated.rs"), "pub fn value() -> u8 { 2 }")
        .await
        .expect("unrelated mid-run source update");

    let proof_accepted = ledger
        .record_verify_local(
            "final",
            Some("VERIFIED"),
            true,
            true,
            Some(&validation_start),
            &[PathBuf::from("src/step.rs")],
            &[],
            Some(&serde_json::json!({"verdict": "VERIFIED"})),
        )
        .await;

    assert!(proof_accepted);
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.validation_epoch, Some(document.evidence_epoch));
    assert!(!document.latest_file_hashes.contains_key("src/unrelated.rs"));
}

#[tokio::test]
async fn validation_rejects_newly_discovered_active_file_that_changes_mid_run() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("src");
    tokio::fs::write(repo.join("src/discovered.rs"), "pub fn value() -> u8 { 1 }")
        .await
        .expect("new dirty source");

    let validation_start = ledger
        .begin_verify_local_validation(&[])
        .await
        .expect("validation start");
    assert!(
        validation_start
            .file_snapshots
            .contains_key("src/discovered.rs")
    );
    assert!(
        !validation_start
            .owned_file_paths
            .contains("src/discovered.rs")
    );
    tokio::fs::write(repo.join("src/discovered.rs"), "pub fn value() -> u8 { 2 }")
        .await
        .expect("mid-run source update");

    let proof_accepted = ledger
        .record_verify_local(
            "final",
            Some("VERIFIED"),
            true,
            true,
            Some(&validation_start),
            &[PathBuf::from("src/discovered.rs")],
            &[],
            Some(&serde_json::json!({"verdict": "VERIFIED"})),
        )
        .await;

    assert!(!proof_accepted);
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.validation_epoch, None);
    assert!(
        document
            .risks
            .iter()
            .any(|risk| { risk.id == "verify-local-concurrent-change" && !risk.resolved })
    );
}

#[tokio::test]
async fn snapshot_file_hashes_across_multiple_bounded_chunks() {
    let (_temp, repo, _ledger) = ledger_fixture().await;
    let bytes = (0..FILE_HASH_CHUNK_SIZE * 2 + 17)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    tokio::fs::write(repo.join("large.bin"), &bytes)
        .await
        .expect("large fixture");

    let snapshot = snapshot_file(&repo, "large.bin").await;

    assert_eq!(snapshot.sha1, Some(sha1_hex(&bytes)));
    assert!(snapshot.exists);
    assert_eq!(snapshot.read_error, None);
}

#[tokio::test]
async fn older_persistence_snapshot_is_reported_as_superseded() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let document = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .clone();
    let mut older = document.clone();
    older.revision = document.revision.saturating_add(1);
    let mut newer = document;
    newer.revision = older.revision.saturating_add(1);

    assert_eq!(
        ledger.persist_document(&newer).await,
        PersistOutcome::Persisted
    );
    assert_eq!(
        ledger.persist_document(&older).await,
        PersistOutcome::Superseded
    );
}

#[test]
fn unreadable_and_artifact_risk_ids_are_stable() {
    assert_eq!(
        unreadable_file_risk_id("src\\step.rs"),
        unreadable_file_risk_id("src/step.rs")
    );
    assert_eq!(
        generated_artifact_freshness_risk_id("generated\\schema.json"),
        generated_artifact_freshness_risk_id("generated/schema.json")
    );
    assert!(edit_outcome_succeeded("completed"));
    assert!(!edit_outcome_succeeded(" completed "));
    assert!(!edit_outcome_succeeded("failed"));
}
