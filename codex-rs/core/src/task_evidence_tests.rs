use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

async fn install_wiring_guard_fixture(codex_home: &Path) -> PathBuf {
    let root = codex_home
        .join("plugins/cache/local-wiring-guards/wiring-guard")
        .join(WIRING_GUARD_PLUGIN_VERSION);
    tokio::fs::create_dir_all(root.join("runtime"))
        .await
        .expect("wiring runtime");
    tokio::fs::create_dir_all(root.join("schemas"))
        .await
        .expect("wiring schemas");
    let launcher = root.join("runtime/wiring_guard.py");
    let launcher_bytes = b"# trusted wiring guard fixture\n";
    tokio::fs::write(&launcher, launcher_bytes)
        .await
        .expect("wiring launcher");
    tokio::fs::write(
        root.join("bundle-manifest.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema_id": "wiring-guard/bundle-manifest",
            "schema_version": "1.0.0",
            "plugin": {"name": "wiring-guard", "version": WIRING_GUARD_PLUGIN_VERSION},
            "files": [{
                "path": "runtime/wiring_guard.py",
                "sha256": sha256_hex(launcher_bytes),
                "size": launcher_bytes.len()
            }]
        }))
        .expect("wiring manifest json"),
    )
    .await
    .expect("wiring manifest");
    tokio::fs::write(
        root.join("schemas/ledger.schema.json"),
        serde_json::to_vec(&serde_json::json!({
            "$defs": {"entry": {"properties": {
                "schema_version": {"const": WIRING_GUARD_LEDGER_SCHEMA_VERSION},
                "report_schema_version": {"const": WIRING_GUARD_REPORT_SCHEMA_VERSION}
            }}}
        }))
        .expect("wiring schema json"),
    )
    .await
    .expect("wiring schema");
    launcher
}

async fn ledger_fixture() -> (tempfile::TempDir, PathBuf, TaskEvidenceLedger) {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let codex_home = temp.path().join("home");
    install_wiring_guard_fixture(&codex_home).await;
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
            "failed-command",
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
        .begin_verify_local_validation()
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
        .begin_verify_local_validation()
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
    document.wiring_receipt = Some(EpochReceipt {
        receipt_id: "command-1".to_string(),
        epoch: 0,
        recorded_at: timestamp(),
        wiring_proof: None,
    });

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
    assert!(document.wiring_receipt.is_none());
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
        .begin_verify_local_validation()
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
async fn validation_rejects_a_newly_discovered_active_file_that_changes_mid_run() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    tokio::fs::create_dir_all(repo.join("src"))
        .await
        .expect("src");
    tokio::fs::write(repo.join("src/discovered.rs"), "pub fn value() -> u8 { 1 }")
        .await
        .expect("new dirty source");

    let validation_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("validation start");
    assert!(
        validation_start
            .file_snapshots
            .contains_key("src/discovered.rs"),
        "the pre-run token must include dirty files not already known to task evidence"
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

#[tokio::test]
async fn wiring_invocation_requires_the_trusted_current_launcher() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let trusted_root = ledger
        .trusted_wiring_guard_root
        .as_deref()
        .expect("trusted wiring root");
    let trusted_launcher = trusted_root.join("runtime/wiring_guard.py");
    assert!(
        trusted_wiring_guard_check_invocation(
            &[
                "echo".to_string(),
                trusted_launcher.to_string_lossy().into_owned(),
                "check".to_string(),
                "--ledger".to_string(),
            ],
            Some(trusted_root),
        )
        .is_none()
    );
    assert!(
        trusted_wiring_guard_check_invocation(
            &[
                "python".to_string(),
                trusted_launcher.to_string_lossy().into_owned(),
                "check".to_string(),
                "--ledger".to_string(),
            ],
            Some(trusted_root),
        )
        .is_some()
    );
    assert!(
        trusted_wiring_guard_check_invocation(
            &[
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "&".to_string(),
                trusted_launcher.to_string_lossy().into_owned(),
                "check".to_string(),
                "--ledger".to_string(),
            ],
            Some(trusted_root),
        )
        .is_some()
    );
    assert!(
        trusted_wiring_guard_check_invocation(
            &[
                "python".to_string(),
                trusted_launcher.to_string_lossy().into_owned(),
                "check".to_string(),
                "--ledger".to_string(),
                ";".to_string(),
                "python".to_string(),
                "forge_ledger.py".to_string(),
            ],
            Some(trusted_root),
        )
        .is_none()
    );

    let trusted_size = tokio::fs::metadata(&trusted_launcher)
        .await
        .expect("trusted launcher metadata")
        .len();
    tokio::fs::write(&trusted_launcher, vec![b'x'; trusted_size as usize])
        .await
        .expect("tampered trusted launcher");
    assert!(
        trusted_wiring_guard_check_invocation(
            &[
                "python".to_string(),
                trusted_launcher.to_string_lossy().into_owned(),
                "check".to_string(),
                "--ledger".to_string(),
            ],
            Some(trusted_root),
        )
        .is_none()
    );

    let untrusted_launcher = repo.join("wiring_guard.py");
    tokio::fs::write(&untrusted_launcher, "# untrusted")
        .await
        .expect("untrusted launcher");
    assert!(
        trusted_wiring_guard_check_invocation(
            &[
                "python".to_string(),
                untrusted_launcher.to_string_lossy().into_owned(),
                "check".to_string(),
                "--ledger".to_string(),
            ],
            Some(trusted_root),
        )
        .is_none()
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
