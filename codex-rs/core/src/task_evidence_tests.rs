use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

#[cfg(windows)]
fn trusted_noop_command() -> Vec<String> {
    vec!["where.exe".to_string()]
}

#[cfg(not(windows))]
fn trusted_noop_command() -> Vec<String> {
    vec!["true".to_string()]
}

#[cfg(windows)]
fn trusted_exact_write_command(path: &str) -> Vec<String> {
    vec![
        "fsutil.exe".to_string(),
        "file".to_string(),
        "createnew".to_string(),
        path.to_string(),
        "0".to_string(),
    ]
}

#[cfg(not(windows))]
fn trusted_exact_write_command(path: &str) -> Vec<String> {
    vec!["touch".to_string(), path.to_string()]
}

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
    let output = isolated_git_command(repo)
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

async fn run_git(repo: &Path, args: &[&str]) {
    let output = isolated_git_command(repo)
        .args(args)
        .output()
        .await
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let output = isolated_git_command(repo)
        .args(args)
        .output()
        .await
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output should be UTF-8")
        .trim()
        .to_string()
}

fn isolated_git_command(repo: &Path) -> tokio::process::Command {
    let null_config = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=.git/no-hooks",
        ])
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", null_config)
        .env("GIT_CONFIG_GLOBAL", null_config);
    command
}

fn repo_uri(repo: &Path) -> PathUri {
    PathUri::from_host_native_path(repo).expect("repository URI")
}

#[tokio::test]
async fn deferred_ledger_creates_no_artifact_until_first_tracked_mutation() {
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
    let thread_id = ThreadId::new();
    let evidence_path = codex_home
        .join("task-evidence")
        .join(format!("{thread_id}.json"));

    let ledger = TaskEvidenceLedger::deferred(codex_home, thread_id, repo.as_path()).await;

    assert!(!evidence_path.exists());
    assert!(!ledger.is_initialized().await);
    assert!(ledger.completion_gate().await.is_none());
    assert_eq!(ledger.take_automatic_verify_plan_request().await, None);
    assert!(!ledger.is_initialized().await);
    assert!(!evidence_path.exists());
    let command = trusted_noop_command();
    let cwd = repo_uri(&repo);
    ledger
        .record_command_intent("read-only", &command, &cwd)
        .await;
    let _ = ledger
        .record_command(
            "read-only",
            &command,
            &cwd,
            0,
            false,
            1,
            /*possible_mutation*/ false,
        )
        .await;
    assert!(!ledger.is_initialized().await);
    assert!(!evidence_path.exists());

    ledger
        .record_plan_update(&UpdatePlanArgs {
            explanation: None,
            plan: vec![plan_item("lazy-start", StepStatus::Pending)],
        })
        .await;
    let revision = ledger.current_revision().await.expect("evidence revision");
    assert_eq!(
        ledger.persist_revision(revision).await,
        PersistOutcome::Persisted
    );
    assert!(evidence_path.exists());
}

#[tokio::test]
async fn deferred_resume_loads_existing_prestate_before_first_command_intent() {
    let (temp, repo, ledger) = ledger_fixture().await;
    record_observed_command_change(&ledger, &repo, "seed-resume").await;
    let (thread_id, persisted_epoch, revision) = {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        (
            document.thread_id.clone(),
            document.evidence_epoch,
            document.revision,
        )
    };
    assert_eq!(
        ledger.persist_revision(revision).await,
        PersistOutcome::Persisted
    );
    drop(ledger);

    let resumed = TaskEvidenceLedger::deferred(
        temp.path().join("home"),
        ThreadId::from_string(&thread_id).expect("thread id"),
        &repo,
    )
    .await;
    tokio::fs::write(repo.join("resumed.txt"), "before")
        .await
        .expect("seed resumed file");
    let cwd = repo_uri(&repo);
    let command = trusted_exact_write_command("resumed.txt");

    resumed
        .record_command_intent("first-resumed-command", &command, &cwd)
        .await;
    tokio::fs::write(repo.join("resumed.txt"), "after")
        .await
        .expect("change resumed file");
    assert_eq!(
        resumed
            .record_command(
                "first-resumed-command",
                &command,
                &cwd,
                0,
                false,
                1,
                true,
            )
            .await,
        Some(MutationObservation::Changed)
    );

    let guard = resumed.document.lock().await;
    let document = guard.as_ref().expect("resumed document");
    let receipt = document
        .command_receipts
        .last()
        .expect("resumed command receipt");
    assert_eq!(receipt.observation, Some(MutationObservation::Changed));
    assert_eq!(receipt.coverage, Some(MutationCoverage::Complete));
    assert_eq!(receipt.epoch, persisted_epoch.saturating_add(1));
}

#[tokio::test]
async fn deferred_resume_honors_persisted_gate_and_automatic_planning_before_mutation() {
    let (temp, repo, ledger) = ledger_fixture().await;
    record_observed_command_change(&ledger, &repo, "seed-planning-resume").await;
    let (thread_id, revision) = {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        (document.thread_id.clone(), document.revision)
    };
    assert_eq!(
        ledger.persist_revision(revision).await,
        PersistOutcome::Persisted
    );
    drop(ledger);

    let resumed = TaskEvidenceLedger::deferred(
        temp.path().join("home"),
        ThreadId::from_string(&thread_id).expect("thread id"),
        &repo,
    )
    .await;
    assert!(!resumed.is_initialized().await);

    let gate = resumed.completion_gate().await.expect("persisted completion gate");
    assert_ne!(gate.status, TaskCompletionStatus::Passed);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason == "verify_local planning is missing or stale")
    );
    assert!(
        resumed
            .take_automatic_verify_plan_request()
            .await
            .is_some(),
        "persisted mutation should request automatic planning"
    );
}

#[tokio::test]
async fn deferred_ledger_preserves_git_identity_from_session_start() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let codex_home = temp.path().join("home");
    tokio::fs::create_dir_all(repo.join("scripts"))
        .await
        .expect("scripts");
    tokio::fs::write(repo.join("scripts/verify_local.py"), "# fixture")
        .await
        .expect("verifier");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("manifest");
    initialize_git_repo(&repo).await;
    run_git(&repo, &["add", "."]).await;
    run_git(
        &repo,
        &[
            "-c",
            "user.name=Codex",
            "-c",
            "user.email=codex@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    )
    .await;
    let starting_commit = git_stdout(&repo, &["rev-parse", "HEAD"]).await;
    let thread_id = ThreadId::new();
    let evidence_path = codex_home
        .join("task-evidence")
        .join(format!("{thread_id}.json"));
    let ledger = TaskEvidenceLedger::deferred(codex_home, thread_id, repo.as_path()).await;
    assert!(!evidence_path.exists());

    tokio::fs::write(repo.join("after-start.txt"), "changed after session start")
        .await
        .expect("second commit file");
    run_git(&repo, &["add", "."]).await;
    run_git(
        &repo,
        &[
            "-c",
            "user.name=Codex",
            "-c",
            "user.email=codex@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "after-start",
        ],
    )
    .await;
    assert_ne!(
        starting_commit,
        git_stdout(&repo, &["rev-parse", "HEAD"]).await
    );

    ledger
        .record_plan_update(&UpdatePlanArgs {
            explanation: None,
            plan: vec![plan_item("preserve-start", StepStatus::Pending)],
        })
        .await;
    let revision = ledger.current_revision().await.expect("evidence revision");
    assert_eq!(
        ledger.persist_revision(revision).await,
        PersistOutcome::Persisted
    );
    let document: Value = serde_json::from_slice(
        &tokio::fs::read(&evidence_path)
            .await
            .expect("task evidence"),
    )
    .expect("task evidence JSON");
    assert_eq!(
        document["start"]["commit_hash"].as_str(),
        Some(starting_commit.as_str())
    );
}

async fn record_observed_command_change(ledger: &TaskEvidenceLedger, repo: &Path, call_id: &str) {
    initialize_git_repo(repo).await;
    tokio::fs::write(repo.join("observed.txt"), "before")
        .await
        .expect("seed observed file");
    let cwd = repo_uri(repo);
    let command = trusted_exact_write_command("observed.txt");
    ledger.record_command_intent(call_id, &command, &cwd).await;
    tokio::fs::write(repo.join("observed.txt"), "after")
        .await
        .expect("change observed file");
    assert_eq!(
        ledger
            .record_command(call_id, &command, &cwd, 0, false, 1, true)
            .await,
        Some(MutationObservation::Changed)
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
        observation: None,
        coverage: None,
        changed_paths: Vec::new(),
        reconciliation_attempted: false,
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
        workspace_generation: None,
        evidence_generation: None,
        automatic_claim_id: None,
        hash_generation: None,
        spec_generation: None,
        conclusion: Some(ValidationConclusion::Passed),
        active_files: Vec::new(),
        stale_reasons: Vec::new(),
        exact_output_artifacts: Vec::new(),
        payload: None,
    }
}

#[tokio::test]
async fn semantic_noop_plan_update_does_not_advance_or_write() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let update = plan_with(vec![plan_item("stable", StepStatus::Pending)]);
    ledger.record_plan_update(&update).await;
    let revision = ledger.current_revision().await.expect("revision");
    assert_eq!(
        ledger.persist_revision(revision).await,
        PersistOutcome::Persisted
    );
    let persistence = ledger.persistence.as_ref().expect("persistence");
    let writes = persistence.write_count.load(Ordering::Acquire);

    ledger.record_plan_update(&update).await;
    tokio::time::sleep(Duration::from_millis(15)).await;

    assert_eq!(ledger.current_revision().await, Some(revision));
    assert_eq!(persistence.write_count.load(Ordering::Acquire), writes);
}

#[tokio::test]
async fn identical_normalized_plan_after_completion_preserves_gate_revision_and_writes() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    tokio::fs::create_dir_all(repo.join("docs"))
        .await
        .expect("docs directory");
    tokio::fs::write(repo.join("docs/item.md"), "before")
        .await
        .expect("seed documentation");
    let mut item = plan_item("stable-completion", StepStatus::InProgress);
    item.runtime_paths.clear();
    ledger.record_plan_update(&plan_with(vec![item])).await;
    let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo");
    ledger
        .record_edit_intent("docs-edit", cwd.as_path(), &[PathBuf::from("docs/item.md")])
        .await;
    tokio::fs::write(repo.join("docs/item.md"), "after")
        .await
        .expect("update documentation");
    ledger.record_edit_result("docs-edit", "completed").await;
    let plan_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("plan validation start");
    ledger
        .record_verify_local(
            "plan",
            Some("PLANNED"),
            true,
            false,
            true,
            Some(&plan_start),
            &[PathBuf::from("docs/item.md")],
            &[],
            Some(&serde_json::json!({"planned": []})),
        )
        .await;
    let final_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("final validation start");
    assert!(
        ledger
            .record_verify_local(
                "final",
                Some("VERIFIED"),
                true,
                true,
                true,
                Some(&final_start),
                &[PathBuf::from("docs/item.md")],
                &[],
                Some(&serde_json::json!({"verdict": "VERIFIED"})),
            )
            .await
    );
    let gate = ledger.completion_gate().await.expect("completed gate");
    assert_eq!(gate.status, TaskCompletionStatus::Passed);
    let normalized = {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        UpdatePlanArgs {
            explanation: None,
            plan: document.plan.iter().map(plan_item_from_evidence).collect(),
        }
    };
    let revision = ledger.current_revision().await.expect("completed revision");
    let completion = ledger
        .document
        .lock()
        .await
        .as_ref()
        .and_then(|document| document.completion.clone());
    let persistence = ledger.persistence.as_ref().expect("persistence");
    let writes = persistence.write_count.load(Ordering::Acquire);

    ledger.record_plan_update(&normalized).await;
    tokio::time::sleep(Duration::from_millis(15)).await;

    assert_eq!(ledger.current_revision().await, Some(revision));
    assert_eq!(
        ledger
            .document
            .lock()
            .await
            .as_ref()
            .and_then(|document| document.completion.clone()),
        completion
    );
    assert_eq!(persistence.write_count.load(Ordering::Acquire), writes);
}

#[tokio::test]
async fn coalescing_writer_serializes_only_the_latest_requested_revision() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let persistence = ledger.persistence.as_ref().expect("persistence");
    let initial_serializations = persistence.serialization_count.load(Ordering::Acquire);
    let initial_writes = persistence.write_count.load(Ordering::Acquire);

    for id in ["one", "two", "three"] {
        ledger
            .record_plan_update(&plan_with(vec![plan_item(id, StepStatus::Pending)]))
            .await;
    }
    let revision = ledger.current_revision().await.expect("revision");
    assert_eq!(
        ledger.persist_revision(revision).await,
        PersistOutcome::Persisted
    );

    assert_eq!(
        persistence.serialization_count.load(Ordering::Acquire),
        initial_serializations + 1
    );
    assert_eq!(
        persistence.write_count.load(Ordering::Acquire),
        initial_writes + 1
    );
    let persisted: TaskEvidenceDocument = serde_json::from_slice(
        &tokio::fs::read(ledger.evidence_path.as_ref().expect("evidence path"))
            .await
            .expect("evidence file"),
    )
    .expect("evidence JSON");
    assert_eq!(persisted.revision, revision);
    assert_eq!(persisted.plan[0].id, "three");
}

#[tokio::test]
async fn interrupted_atomic_write_recovers_last_durable_document_and_ignores_orphan() {
    let (temp, repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("durable", StepStatus::Pending)]))
        .await;
    let durable_revision = ledger.current_revision().await.expect("durable revision");
    assert_eq!(
        ledger.persist_revision(durable_revision).await,
        PersistOutcome::Persisted
    );
    let evidence_path = ledger.evidence_path.as_ref().expect("evidence path").clone();
    let durable_bytes = tokio::fs::read(&evidence_path)
        .await
        .expect("durable evidence");
    let thread_id = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("document")
        .thread_id
        .clone();
    let persistence = ledger.persistence.as_ref().expect("persistence");
    persistence
        .interrupt_next_write_before_replace
        .store(true, Ordering::Release);

    ledger
        .record_plan_update(&plan_with(vec![plan_item("interrupted", StepStatus::Pending)]))
        .await;
    let interrupted_revision = ledger.current_revision().await.expect("interrupted revision");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let state = persistence.state.lock().await;
            let interrupted = state.failed_revision == Some(interrupted_revision)
                && !state.writer_running;
            drop(state);
            if interrupted {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("injected writer interruption");

    assert_eq!(
        tokio::fs::read(&evidence_path)
            .await
            .expect("canonical evidence after interruption"),
        durable_bytes
    );
    let mut entries = tokio::fs::read_dir(evidence_path.parent().expect("evidence directory"))
        .await
        .expect("evidence directory");
    let mut orphan = None;
    while let Some(entry) = entries.next_entry().await.expect("directory entry") {
        if entry.path() != evidence_path {
            orphan = Some(entry.path());
            break;
        }
    }
    let orphan = orphan.expect("interrupted temporary evidence file");
    assert!(
        serde_json::from_slice::<TaskEvidenceDocument>(
            &tokio::fs::read(orphan).await.expect("orphaned partial write")
        )
        .is_err()
    );
    drop(ledger);

    let resumed = TaskEvidenceLedger::load_or_new(
        temp.path().join("home"),
        ThreadId::from_string(&thread_id).expect("thread id"),
        &repo,
    )
    .await;
    let guard = resumed.document.lock().await;
    let document = guard.as_ref().expect("recovered document");
    assert_eq!(document.plan[0].id, "durable");
    assert!(document.revision > durable_revision);
}

#[tokio::test]
async fn later_mutation_cancels_an_older_validation_generation() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("before", StepStatus::Pending)]))
        .await;
    let validation_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("validation start");

    ledger
        .record_plan_update(&plan_with(vec![plan_item("after", StepStatus::Pending)]))
        .await;

    assert!(validation_start.cancellation_token.is_cancelled());
    assert!(
        !ledger
            .record_verify_local(
                "final",
                Some("VERIFIED"),
                true,
                true,
                true,
                Some(&validation_start),
                &[],
                &[],
                Some(&serde_json::json!({"verdict": "VERIFIED"})),
            )
            .await
    );
    let guard = ledger.document.lock().await;
    let receipt = guard
        .as_ref()
        .expect("document")
        .validation_receipts
        .last()
        .expect("validation receipt");
    assert_eq!(
        receipt.workspace_generation.as_deref(),
        Some(validation_start.workspace_generation.as_str())
    );
    assert_eq!(
        receipt.evidence_generation,
        Some(validation_start.evidence_generation)
    );
    assert_eq!(
        receipt.hash_generation.as_deref(),
        Some(validation_start.hash_generation.as_str())
    );
    assert_eq!(
        receipt.spec_generation.as_deref(),
        Some(validation_start.spec_generation.as_str())
    );
}

#[tokio::test]
async fn unchanged_finalization_reuses_the_persisted_terminal_revision() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item("terminal", StepStatus::Pending)]))
        .await;
    let first_gate = ledger.completion_gate().await.expect("first gate");
    let revision = ledger.current_revision().await.expect("revision");
    let persistence = ledger.persistence.as_ref().expect("persistence");
    let writes = persistence.write_count.load(Ordering::Acquire);

    let second_gate = ledger.completion_gate().await.expect("second gate");

    assert_eq!(second_gate, first_gate);
    assert_eq!(ledger.current_revision().await, Some(revision));
    assert_eq!(persistence.write_count.load(Ordering::Acquire), writes);
    assert_eq!(persistence.state.lock().await.persisted_revision, revision);
}

#[tokio::test]
async fn persisted_command_receipt_is_recovered_exactly_once() {
    let (temp, repo, ledger) = ledger_fixture().await;
    let command = trusted_noop_command();
    let cwd = repo_uri(&repo);
    ledger
        .record_command_intent("receipt", &command, &cwd)
        .await;
    ledger
        .record_command("receipt", &command, &cwd, 0, false, 1, false)
        .await;
    let (thread_id, revision) = {
        let guard = ledger.document.lock().await;
        let document = guard.as_ref().expect("document");
        (document.thread_id.clone(), document.revision)
    };
    assert_eq!(
        ledger.persist_revision(revision).await,
        PersistOutcome::Persisted
    );
    drop(ledger);

    let resumed = TaskEvidenceLedger::load_or_new(
        temp.path().join("home"),
        ThreadId::from_string(&thread_id).expect("thread id"),
        &repo,
    )
    .await;
    let guard = resumed.document.lock().await;
    assert_eq!(guard.as_ref().expect("document").command_receipts.len(), 1);
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
async fn verify_local_receipt_persists_exact_output_artifact_binding() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let exact = format!(
        "{}\nerror: decisive tail failure",
        "ordinary output\n".repeat(5000)
    );
    let artifact_path = repo.join("exact-verify-output.log");
    tokio::fs::write(&artifact_path, exact.as_bytes())
        .await
        .expect("exact verifier artifact");
    let digest = sha256_hex(exact.as_bytes());
    let payload = serde_json::json!({
        "results": [{
            "id": "verify:final",
            "exact_output_artifact": {
                "sha256": digest.clone(),
                "path": artifact_path.to_string_lossy(),
                "bytes": exact.len()
            }
        }]
    });
    let validation_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("validation start");
    ledger
        .record_verify_local(
            "final",
            Some("FAILED"),
            false,
            false,
            true,
            Some(&validation_start),
            &[],
            &[],
            Some(&payload),
        )
        .await;

    let revision = ledger
        .document
        .lock()
        .await
        .as_ref()
        .expect("evidence document")
        .revision;
    assert_eq!(
        ledger.persist_revision(revision).await,
        PersistOutcome::Persisted
    );
    let evidence_path = ledger.evidence_path.as_ref().expect("evidence path");
    let persisted: TaskEvidenceDocument = serde_json::from_slice(
        &tokio::fs::read(evidence_path)
            .await
            .expect("persisted evidence"),
    )
    .expect("persisted evidence document");
    let binding = persisted
        .validation_receipts
        .last()
        .and_then(|receipt| receipt.exact_output_artifacts.first())
        .expect("receipt artifact binding");
    assert_eq!(binding.sha256, digest);
    assert_eq!(binding.bytes, exact.len() as u64);
    let reread = tokio::fs::read(&binding.path)
        .await
        .expect("reread receipt artifact");
    assert!(reread.ends_with(b"error: decisive tail failure"));
    assert_eq!(sha256_hex(&reread), binding.sha256);
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
    {
        let mut guard = ledger.document.lock().await;
        *guard.as_mut().expect("document") = newer.clone();
    }

    assert_eq!(
        ledger.persist_revision(newer.revision).await,
        PersistOutcome::Persisted
    );
    assert_eq!(
        ledger.persist_revision(older.revision).await,
        PersistOutcome::Superseded
    );
}

#[tokio::test]
async fn complete_coverage_noop_is_unchanged_without_advancing_the_epoch() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    let cwd = repo_uri(&repo);
    let command = trusted_noop_command();

    ledger.record_command_intent("noop", &command, &cwd).await;
    let observation = ledger
        .record_command("noop", &command, &cwd, 0, false, 1, false)
        .await;

    assert_eq!(observation, Some(MutationObservation::Unchanged));
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.evidence_epoch, 0);
    let receipt = document.command_receipts.last().expect("command receipt");
    assert_eq!(receipt.coverage, Some(MutationCoverage::Complete));
    assert_eq!(receipt.observation, observation);
    drop(guard);
    assert_eq!(ledger.take_automatic_verify_plan_request().await, None);
}

#[tokio::test]
async fn failed_complete_coverage_noop_does_not_advance_or_plan() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    let cwd = repo_uri(&repo);
    let command = trusted_noop_command();

    ledger
        .record_command_intent("failed-noop", &command, &cwd)
        .await;
    assert_eq!(
        ledger
            .record_command("failed-noop", &command, &cwd, 1, false, 1, true)
            .await,
        Some(MutationObservation::Unchanged)
    );
    assert_eq!(
        ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .evidence_epoch,
        0
    );
    assert_eq!(ledger.take_automatic_verify_plan_request().await, None);
}

#[tokio::test]
async fn identical_content_write_does_not_advance_or_plan() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    tokio::fs::write(repo.join("same.txt"), "same")
        .await
        .expect("seed unchanged file");
    let cwd = repo_uri(&repo);
    let command = trusted_exact_write_command("same.txt");

    ledger
        .record_command_intent("same-write", &command, &cwd)
        .await;
    assert_eq!(
        ledger
            .record_command("same-write", &command, &cwd, 0, false, 1, true)
            .await,
        Some(MutationObservation::Unchanged)
    );
    assert_eq!(
        ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .evidence_epoch,
        0
    );
    assert_eq!(ledger.take_automatic_verify_plan_request().await, None);
}

#[tokio::test]
async fn git_failure_keeps_complete_syntax_observation_unknown() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    let cwd = repo_uri(&repo);
    let command = trusted_noop_command();

    ledger
        .record_command_intent("git-failure", &command, &cwd)
        .await;
    assert_eq!(
        ledger
            .record_command("git-failure", &command, &cwd, 0, false, 1, false)
            .await,
        Some(MutationObservation::Unknown)
    );
}

#[tokio::test]
async fn incomplete_coverage_noop_is_unknown_without_advancing_the_epoch() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    let cwd = repo_uri(&repo);
    let command = vec!["echo".to_string(), "ok".to_string()];

    ledger
        .record_command_intent("unknown-noop", &command, &cwd)
        .await;
    let observation = ledger
        .record_command("unknown-noop", &command, &cwd, 0, false, 1, false)
        .await;

    assert_eq!(observation, Some(MutationObservation::Unknown));
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.evidence_epoch, 0);
    assert_eq!(
        document
            .command_receipts
            .last()
            .and_then(|receipt| receipt.coverage),
        Some(MutationCoverage::Incomplete)
    );
    assert!(
        document
            .risks
            .iter()
            .any(|risk| risk.source == "command" && !risk.resolved)
    );
}

#[tokio::test]
async fn complete_coverage_detects_existing_untracked_content_changes_even_on_failure() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    tokio::fs::write(repo.join("existing.txt"), "before")
        .await
        .expect("write untracked file");
    let cwd = repo_uri(&repo);
    let command = trusted_exact_write_command("existing.txt");

    ledger
        .record_command_intent("untracked-change", &command, &cwd)
        .await;
    tokio::fs::write(repo.join("existing.txt"), "after")
        .await
        .expect("modify untracked file");
    let observation = ledger
        .record_command("untracked-change", &command, &cwd, 1, false, 1, true)
        .await;

    assert_eq!(observation, Some(MutationObservation::Changed));
    assert_eq!(
        ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .evidence_epoch,
        1
    );
    assert_eq!(
        ledger
            .take_automatic_verify_plan_request()
            .await
            .map(|request| request.changed_paths),
        Some(Vec::new())
    );
}

#[tokio::test]
async fn complete_coverage_detects_changes_to_an_already_dirty_tracked_file() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    tokio::fs::write(repo.join("tracked.txt"), "clean")
        .await
        .expect("tracked file");
    run_git(&repo, &["add", "tracked.txt"]).await;
    run_git(
        &repo,
        &[
            "-c",
            "user.name=KD4 Test",
            "-c",
            "user.email=kd4@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    )
    .await;
    tokio::fs::write(repo.join("tracked.txt"), "dirty-before")
        .await
        .expect("initial dirty content");
    let cwd = repo_uri(&repo);
    let command = trusted_exact_write_command("tracked.txt");

    ledger
        .record_command_intent("dirty-tracked-change", &command, &cwd)
        .await;
    tokio::fs::write(repo.join("tracked.txt"), "dirty-after")
        .await
        .expect("changed dirty content");

    assert_eq!(
        ledger
            .record_command("dirty-tracked-change", &command, &cwd, 1, false, 1, true,)
            .await,
        Some(MutationObservation::Changed)
    );
}

#[tokio::test]
async fn fingerprint_is_content_sensitive_for_dirty_index_and_head_state() {
    let (_temp, repo, _ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    tokio::fs::write(repo.join("tracked.txt"), "one")
        .await
        .expect("tracked file");
    run_git(&repo, &["add", "tracked.txt"]).await;
    run_git(
        &repo,
        &[
            "-c",
            "user.name=KD4 Test",
            "-c",
            "user.email=kd4@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    )
    .await;
    let artifacts = BTreeSet::new();
    let clean = capture_stable_mutation_fingerprint(&repo, &artifacts)
        .await
        .expect("clean fingerprint");

    tokio::fs::write(repo.join("tracked.txt"), "two")
        .await
        .expect("first dirty content");
    let dirty = capture_stable_mutation_fingerprint(&repo, &artifacts)
        .await
        .expect("dirty fingerprint");
    assert_ne!(clean, dirty);

    tokio::fs::write(repo.join("tracked.txt"), "three")
        .await
        .expect("second dirty content");
    let dirtier = capture_stable_mutation_fingerprint(&repo, &artifacts)
        .await
        .expect("content-sensitive dirty fingerprint");
    assert_ne!(dirty, dirtier);

    run_git(&repo, &["add", "tracked.txt"]).await;
    let staged = capture_stable_mutation_fingerprint(&repo, &artifacts)
        .await
        .expect("staged fingerprint");
    assert_ne!(dirtier, staged);

    run_git(
        &repo,
        &[
            "-c",
            "user.name=KD4 Test",
            "-c",
            "user.email=kd4@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "second",
        ],
    )
    .await;
    let committed = capture_stable_mutation_fingerprint(&repo, &artifacts)
        .await
        .expect("committed fingerprint");
    assert_ne!(staged, committed);
}

#[tokio::test]
async fn fingerprint_ignores_configured_pagers_external_diff_and_textconv() {
    let (_temp, repo, _ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    tokio::fs::write(repo.join(".gitattributes"), "*.txt diff=kd4\n")
        .await
        .expect("attributes");
    tokio::fs::write(repo.join("tracked.txt"), "one")
        .await
        .expect("tracked file");
    run_git(&repo, &["add", ".gitattributes", "tracked.txt"]).await;
    run_git(
        &repo,
        &[
            "-c",
            "user.name=KD4 Test",
            "-c",
            "user.email=kd4@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    )
    .await;
    tokio::fs::write(repo.join("tracked.txt"), "two")
        .await
        .expect("dirty tracked file");
    let artifacts = BTreeSet::new();
    let baseline = capture_stable_mutation_fingerprint(&repo, &artifacts)
        .await
        .expect("baseline fingerprint");

    run_git(&repo, &["config", "color.ui", "always"]).await;
    run_git(&repo, &["config", "core.pager", "definitely-not-a-pager"]).await;
    run_git(
        &repo,
        &["config", "diff.external", "definitely-not-an-external-diff"],
    )
    .await;
    run_git(
        &repo,
        &["config", "diff.kd4.textconv", "definitely-not-a-textconv"],
    )
    .await;
    run_git(&repo, &["config", "core.autocrlf", "true"]).await;
    run_git(&repo, &["config", "core.quotePath", "true"]).await;
    run_git(&repo, &["config", "diff.submodule", "log"]).await;
    run_git(&repo, &["config", "diff.ignoreSubmodules", "all"]).await;
    run_git(&repo, &["config", "diff.orderFile", "missing-order-file"]).await;

    let configured = capture_stable_mutation_fingerprint(&repo, &artifacts)
        .await
        .expect("fixed invocation must ignore configurable presentation helpers");
    assert_eq!(configured, baseline);
}

#[tokio::test]
async fn fingerprint_rejects_a_covered_root_nested_below_git_toplevel() {
    let temp = tempfile::tempdir().expect("tempdir");
    let parent = temp.path().join("parent");
    let nested = parent.join("nested");
    tokio::fs::create_dir_all(&nested)
        .await
        .expect("nested root");
    initialize_git_repo(&parent).await;

    assert_eq!(
        capture_stable_mutation_fingerprint(&nested, &BTreeSet::new()).await,
        None
    );
}

#[tokio::test]
async fn ignored_target_requires_exact_registered_artifact_coverage() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    tokio::fs::write(repo.join(".gitignore"), "ignored.txt\n")
        .await
        .expect("gitignore");
    let cwd = repo_uri(&repo);
    let command = trusted_exact_write_command("ignored.txt");

    ledger
        .record_command_intent("ignored-unknown", &command, &cwd)
        .await;
    tokio::fs::write(repo.join("ignored.txt"), "created")
        .await
        .expect("ignored file");
    assert_eq!(
        ledger
            .record_command("ignored-unknown", &command, &cwd, 0, false, 1, true)
            .await,
        Some(MutationObservation::Unknown)
    );

    {
        let mut guard = ledger.document.lock().await;
        let document = guard.as_mut().expect("document");
        document
            .generated_artifact_requirements
            .push(GeneratedArtifactRequirement {
                id: "ignored-artifact".to_string(),
                step_id: None,
                path: Some("ignored.txt".to_string()),
                validation_command: Vec::new(),
                source: "test".to_string(),
                validation_receipt_ids: Vec::new(),
            });
        document.revision = document.revision.saturating_add(1);
    }
    ledger
        .record_command_intent("ignored-known", &command, &cwd)
        .await;
    tokio::fs::write(repo.join("ignored.txt"), "modified")
        .await
        .expect("modify ignored artifact");
    assert_eq!(
        ledger
            .record_command("ignored-known", &command, &cwd, 0, false, 1, true)
            .await,
        Some(MutationObservation::Changed)
    );
}

#[tokio::test]
async fn dynamic_touch_target_is_unknown() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    let cwd = repo_uri(&repo);
    let command = vec!["touch".to_string(), "*.txt".to_string()];

    ledger
        .record_command_intent("dynamic", &command, &cwd)
        .await;
    assert_eq!(
        ledger
            .record_command("dynamic", &command, &cwd, 0, false, 1, true)
            .await,
        Some(MutationObservation::Unknown)
    );
}

#[tokio::test]
async fn untrusted_touch_executable_is_unknown() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    let executable = repo.join(if cfg!(windows) { "touch.exe" } else { "touch" });
    tokio::fs::write(&executable, b"untrusted fixture")
        .await
        .expect("write untrusted executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = tokio::fs::metadata(&executable)
            .await
            .expect("untrusted executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        tokio::fs::set_permissions(&executable, permissions)
            .await
            .expect("make fixture executable");
    }
    let cwd = repo_uri(&repo);
    let command = vec![
        executable.to_string_lossy().into_owned(),
        "outside-model.txt".to_string(),
    ];

    ledger
        .record_command_intent("untrusted-touch", &command, &cwd)
        .await;
    assert_eq!(
        ledger
            .record_command("untrusted-touch", &command, &cwd, 0, false, 1, true)
            .await,
        Some(MutationObservation::Unknown)
    );
}

#[test]
fn legacy_receipts_default_new_evidence_fields_conservatively() {
    let mut command =
        serde_json::to_value(command_receipt("legacy-command")).expect("serialize command receipt");
    command
        .as_object_mut()
        .expect("command object")
        .remove("observation");
    command
        .as_object_mut()
        .expect("command object")
        .remove("coverage");
    let command: CommandReceipt =
        serde_json::from_value(command).expect("deserialize legacy command receipt");
    assert_eq!(command.observation, None);
    assert_eq!(command.coverage, None);

    let mut validation = serde_json::to_value(validation_receipt("legacy-validation"))
        .expect("serialize validation receipt");
    validation
        .as_object_mut()
        .expect("validation object")
        .remove("conclusion");
    let validation: ValidationReceipt =
        serde_json::from_value(validation).expect("deserialize legacy validation receipt");
    assert_eq!(validation.conclusion, None);
}

#[tokio::test]
async fn final_failure_suppresses_later_fast_pass_for_the_epoch() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let final_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("final start");
    assert!(
        !ledger
            .record_verify_local(
                "final",
                Some("FAILED"),
                false,
                false,
                true,
                Some(&final_start),
                &[],
                &[],
                Some(&serde_json::json!({"verdict": "FAILED"})),
            )
            .await
    );
    let fast_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("fast start");
    assert!(
        !ledger
            .record_verify_local(
                "fast",
                Some("VERIFIED"),
                true,
                true,
                true,
                Some(&fast_start),
                &[],
                &[],
                Some(&serde_json::json!({"verdict": "VERIFIED"})),
            )
            .await
    );

    assert_eq!(
        ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .validation_epoch,
        None
    );
}

#[tokio::test]
async fn inconclusive_final_does_not_suppress_fast_pass() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let final_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("final start");
    ledger
        .record_verify_local(
            "final",
            Some("INCONCLUSIVE"),
            false,
            false,
            true,
            Some(&final_start),
            &[],
            &[],
            Some(&serde_json::json!({"verdict": "INCONCLUSIVE"})),
        )
        .await;
    let fast_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("fast start");
    assert!(
        ledger
            .record_verify_local(
                "fast",
                Some("VERIFIED"),
                true,
                true,
                true,
                Some(&fast_start),
                &[],
                &[],
                Some(&serde_json::json!({"verdict": "VERIFIED"})),
            )
            .await
    );
}

#[tokio::test]
async fn failed_verdict_without_conclusive_completion_suppresses_nothing() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let final_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("final start");
    ledger
        .record_verify_local(
            "final",
            Some("FAILED"),
            false,
            false,
            false,
            Some(&final_start),
            &[],
            &[],
            Some(&serde_json::json!({"verdict": "FAILED"})),
        )
        .await;
    let fast_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("fast start");
    assert!(
        ledger
            .record_verify_local(
                "fast",
                Some("VERIFIED"),
                true,
                true,
                true,
                Some(&fast_start),
                &[],
                &[],
                Some(&serde_json::json!({"verdict": "VERIFIED"})),
            )
            .await
    );
}

#[tokio::test]
async fn conclusive_fast_suppresses_later_plan_for_the_epoch() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let fast_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("fast start");
    ledger
        .record_verify_local(
            "fast",
            Some("FAILED"),
            false,
            false,
            true,
            Some(&fast_start),
            &[],
            &[],
            Some(&serde_json::json!({"verdict": "FAILED"})),
        )
        .await;
    let plan_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("plan start");
    assert!(
        !ledger
            .record_verify_local(
                "plan",
                Some("VERIFIED"),
                true,
                true,
                true,
                Some(&plan_start),
                &[],
                &[],
                Some(&serde_json::json!({"verdict": "VERIFIED"})),
            )
            .await
    );
    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.validation_epoch, None);
    assert_eq!(document.verify_plan_epoch, None);
}

#[tokio::test]
async fn authoritative_final_receipt_survives_history_trimming() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let mut guard = ledger.document.lock().await;
    let document = guard.as_mut().expect("document");
    let mut final_receipt = validation_receipt("authoritative-final");
    final_receipt.conclusion = Some(ValidationConclusion::Failed);
    document.validation_receipts.push(final_receipt);
    for index in 0..=MAX_VALIDATION_RECEIPTS {
        let mut receipt = validation_receipt(&format!("later-plan-{index}"));
        receipt.mode = "plan".to_string();
        receipt.conclusion = None;
        document.validation_receipts.push(receipt);
    }

    trim_validation_receipts(document);

    assert_eq!(document.validation_receipts.len(), MAX_VALIDATION_RECEIPTS);
    assert!(
        document
            .validation_receipts
            .iter()
            .any(|receipt| receipt.id == "authoritative-final")
    );
    assert_eq!(
        strongest_conclusive_validation_strength(document, document.evidence_epoch),
        Some(validation_mode_strength("final"))
    );
}

#[tokio::test]
async fn final_pass_supersedes_fast_failure_and_late_plan_is_suppressed() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    let fast_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("fast start");
    ledger
        .record_verify_local(
            "fast",
            Some("FAILED"),
            false,
            false,
            true,
            Some(&fast_start),
            &[],
            &[],
            Some(&serde_json::json!({"verdict": "FAILED"})),
        )
        .await;
    let final_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("final start");
    assert!(
        ledger
            .record_verify_local(
                "final",
                Some("VERIFIED"),
                true,
                true,
                true,
                Some(&final_start),
                &[],
                &[],
                Some(&serde_json::json!({"verdict": "VERIFIED"})),
            )
            .await
    );
    let plan_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("plan start");
    ledger
        .record_verify_local(
            "plan",
            Some("PLANNED"),
            true,
            false,
            true,
            Some(&plan_start),
            &[],
            &[],
            Some(&serde_json::json!({"planned": []})),
        )
        .await;

    let guard = ledger.document.lock().await;
    let document = guard.as_ref().expect("document");
    assert_eq!(document.validation_epoch, Some(document.evidence_epoch));
    assert_eq!(document.verify_plan_epoch, None);
    assert!(
        document
            .risks
            .iter()
            .filter(|risk| risk.source == "verify_local")
            .all(|risk| risk.resolved)
    );
}

#[tokio::test]
async fn successful_fast_or_final_validation_suppresses_automatic_planning() {
    for mode in ["fast", "final"] {
        let (_temp, repo, ledger) = ledger_fixture().await;
        record_observed_command_change(&ledger, &repo, mode).await;
        let validation_start = ledger
            .begin_verify_local_validation()
            .await
            .expect("validation start");

        assert!(
            ledger
                .record_verify_local(
                    mode,
                    Some("VERIFIED"),
                    true,
                    true,
                    true,
                    Some(&validation_start),
                    &[],
                    &[],
                    Some(&serde_json::json!({"verdict": "VERIFIED"})),
                )
                .await
        );
        assert_eq!(ledger.take_automatic_verify_plan_request().await, None);
        let gate = ledger.completion_gate().await.expect("completion gate");
        assert!(
            gate.reasons
                .iter()
                .all(|reason| reason != "verify_local planning is missing or stale")
        );
    }
}

#[tokio::test]
async fn automatic_validation_claim_is_generation_bound_and_retriable_after_setup_failure() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    record_observed_command_change(&ledger, &repo, "automatic-generation-one").await;
    let first = ledger
        .take_automatic_verify_plan_request()
        .await
        .expect("first automatic validation claim");
    ledger
        .release_automatic_verify_plan_request(first.evidence_generation, &first.claim_id)
        .await;
    let retry = ledger
        .take_automatic_verify_plan_request()
        .await
        .expect("released automatic validation claim");
    assert_eq!(retry.evidence_generation, first.evidence_generation);
    assert_ne!(retry.claim_id, first.claim_id);

    record_observed_command_change(&ledger, &repo, "automatic-generation-two").await;
    assert!(
        ledger
            .begin_verify_local_validation_for_automatic_request(&retry)
            .await
            .is_none(),
        "a stale automatic run must not attach to the newer evidence generation"
    );
    let replacement = ledger
        .take_automatic_verify_plan_request()
        .await
        .expect("replacement automatic validation claim");
    assert!(replacement.evidence_generation > retry.evidence_generation);
}

#[tokio::test]
async fn historical_conclusive_claim_does_not_block_retry_after_current_nonconclusive() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    record_observed_command_change(&ledger, &repo, "claim-sequence").await;
    let first = ledger
        .take_automatic_verify_plan_request()
        .await
        .expect("first automatic claim");
    let first_start = ledger
        .begin_verify_local_validation_for_automatic_request(&first)
        .await
        .expect("first claim validation start");
    assert!(
        !ledger
            .record_verify_local(
                "fast",
                Some("FAILED"),
                false,
                false,
                true,
                Some(&first_start),
                &[PathBuf::from("observed.txt")],
                &[],
                Some(&serde_json::json!({"verdict": "FAILED"})),
            )
            .await
    );
    ledger
        .finish_automatic_verify_local_request(first.evidence_generation, &first.claim_id)
        .await;
    {
        let guard = ledger.document.lock().await;
        let receipt = guard
            .as_ref()
            .expect("document")
            .validation_receipts
            .last()
            .expect("first receipt");
        assert_eq!(receipt.automatic_claim_id.as_deref(), Some(first.claim_id.as_str()));
        assert_eq!(receipt.conclusion, Some(ValidationConclusion::Failed));
    }

    let second = ledger
        .take_automatic_verify_plan_request()
        .await
        .expect("second automatic claim");
    assert_ne!(second.claim_id, first.claim_id);
    let second_start = ledger
        .begin_verify_local_validation_for_automatic_request(&second)
        .await
        .expect("second claim validation start");
    assert!(
        !ledger
            .record_verify_local(
                "fast",
                None,
                false,
                false,
                true,
                Some(&second_start),
                &[PathBuf::from("observed.txt")],
                &[],
                Some(&serde_json::json!({"verdict": "INCONCLUSIVE"})),
            )
            .await
    );
    ledger
        .finish_automatic_verify_local_request(second.evidence_generation, &second.claim_id)
        .await;

    let third = ledger
        .take_automatic_verify_plan_request()
        .await
        .expect("third automatic claim after nonconclusive retry");
    assert_eq!(third.evidence_generation, first.evidence_generation);
    assert_ne!(third.claim_id, first.claim_id);
    assert_ne!(third.claim_id, second.claim_id);
}

#[tokio::test]
async fn command_only_change_schedules_exact_changed_path() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    record_observed_command_change(&ledger, &repo, "exact-command-scope").await;

    let request = ledger
        .take_automatic_verify_plan_request()
        .await
        .expect("automatic validation request");

    assert_eq!(request.changed_paths, vec!["observed.txt".to_string()]);
}

#[tokio::test]
async fn unknown_command_mutations_share_one_reconciliation_before_scheduling() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    initialize_git_repo(&repo).await;
    tokio::fs::write(repo.join("unknown.txt"), "dirty")
        .await
        .expect("dirty file");
    let cwd = repo_uri(&repo);
    let command = trusted_noop_command();
    for call_id in ["unknown-one", "unknown-two"] {
        assert_eq!(
            ledger
                .record_command(call_id, &command, &cwd, 0, false, 1, true)
                .await,
            Some(MutationObservation::Unknown)
        );
    }

    let request = ledger
        .take_automatic_verify_plan_request()
        .await
        .expect("reconciled automatic request");
    assert!(request.changed_paths.contains(&"unknown.txt".to_string()));
    assert_eq!(
        ledger.unknown_reconciliation_count.load(Ordering::Acquire),
        1
    );
    {
        let guard = ledger.document.lock().await;
        let receipts = guard
            .as_ref()
            .expect("document")
            .command_receipts
            .iter()
            .filter(|receipt| receipt.observation == Some(MutationObservation::Unknown))
            .collect::<Vec<_>>();
        assert_eq!(receipts.len(), 2);
        assert!(receipts.iter().all(|receipt| receipt.reconciliation_attempted));
        assert!(
            receipts
                .iter()
                .all(|receipt| receipt.changed_paths == request.changed_paths)
        );
    }
    ledger
        .release_automatic_verify_plan_request(request.evidence_generation, &request.claim_id)
        .await;
    assert!(ledger.take_automatic_verify_plan_request().await.is_some());
    assert_eq!(
        ledger.unknown_reconciliation_count.load(Ordering::Acquire),
        1
    );
}

#[tokio::test]
async fn stronger_final_failure_restores_automatic_planning_after_fast_success() {
    let (_temp, repo, ledger) = ledger_fixture().await;
    record_observed_command_change(&ledger, &repo, "stronger-failure").await;
    assert!(
        ledger
            .take_automatic_verify_plan_request()
            .await
            .is_some(),
        "automatic fast validation should claim the current evidence generation"
    );
    let fast_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("fast validation start");
    assert!(
        ledger
            .record_verify_local(
                "fast",
                Some("VERIFIED"),
                true,
                true,
                true,
                Some(&fast_start),
                &[],
                &[],
                Some(&serde_json::json!({"verdict": "VERIFIED"})),
            )
            .await
    );
    let final_start = ledger
        .begin_verify_local_validation()
        .await
        .expect("final validation start");
    assert!(
        !ledger
            .record_verify_local(
                "final",
                Some("FAILED"),
                false,
                false,
                true,
                Some(&final_start),
                &[],
                &[],
                Some(&serde_json::json!({"verdict": "FAILED"})),
            )
            .await
    );

    assert_eq!(
        ledger
            .take_automatic_verify_plan_request()
            .await
            .map(|request| request.changed_paths),
        Some(vec!["observed.txt".to_string()])
    );
}

#[tokio::test]
async fn finalization_exhaustion_returns_a_conservative_non_pass() {
    let (_temp, _repo, ledger) = ledger_fixture().await;
    ledger
        .record_plan_update(&plan_with(vec![plan_item(
            "unstable",
            StepStatus::Implemented,
        )]))
        .await;
    let revision = ledger.current_revision().await.expect("revision");
    assert_eq!(
        ledger.persist_revision(revision).await,
        PersistOutcome::Persisted
    );
    let persistence = ledger.persistence.as_ref().expect("persistence");
    persistence.state.lock().await.persisted_revision = u64::MAX;

    let gate = ledger.completion_gate().await.expect("completion gate");

    assert_ne!(gate.status, TaskCompletionStatus::Passed);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| { reason.contains("evidence changed during finalization") })
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
