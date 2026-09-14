//! Runner integration uses a local protocol peer, never a provider/model call.
#![cfg(windows)]

use super::*;
use crate::prepare::builds::BuildIdentity;
use crate::prepare::environment::{BASE_CONFIG, Environment, ToolIdentity};
use crate::prepare::provenance::{FileIdentity, read_json};
use crate::prepare::{FrozenFixture, MANIFEST_VERSION, SourceIdentity};
use crate::schedule::{Mode, Variant, schedule};
use crate::workloads::{LiveTask, prepare_fixture};
use serde_json::Value;

#[test]
fn temporary_authentication_is_removed_after_failed_preparation_and_native_creation() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source-auth.json");
    let home = temp.path().join("isolated-home");
    fs::create_dir(&home).unwrap();
    let destination = home.join("auth.json");
    let secret = b"{\"access_token\":\"synthetic-test-credential\"}";
    fs::write(&source, secret).unwrap();
    let binary = temp.path().join("native-artifact");
    fs::write(&binary, b"prepared binary").unwrap();
    let identity = FileIdentity::record(&binary).unwrap();
    fs::write(&binary, b"changed binary").unwrap();
    let result = (|| -> Result<()> {
        let _authentication = TemporaryAuthentication::install(Some(&source), &destination)?;
        assert_eq!(fs::read(&destination).unwrap(), secret);
        identity.verify()?;
        Ok(())
    })();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("changed prepared artifact")
    );
    assert!(
        !destination.exists(),
        "failed preparation retained copied authentication"
    );
    assert_eq!(
        fs::read(&source).unwrap(),
        secret,
        "source authentication was changed"
    );

    // A native process may create/refresh auth even when no source file existed.
    let mut authentication = TemporaryAuthentication::install(None, &destination).unwrap();
    fs::write(&destination, b"new synthetic native credential").unwrap();
    authentication.remove().unwrap();
    assert!(
        !destination.exists(),
        "native-created authentication survived cleanup"
    );
    authentication.remove().unwrap();
}

fn prepared_peer(root: &Path, startup_failure: bool) -> Prepared {
    let directory = root.join("prepared");
    let frozen = directory.join("frozen");
    fs::create_dir_all(&frozen).unwrap();
    let directory = fs::canonicalize(directory).unwrap();
    let workspace = directory.join("workspace");
    let snapshot = directory.join("fixtures/scripted");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(
        snapshot.join("AGENTS.md"),
        "Run the requested focused task.\n",
    )
    .unwrap();
    fs::write(
        snapshot.join("benchmark-input.txt"),
        "Repo Benchmark deterministic input\n",
    )
    .unwrap();
    let config = frozen.join("config.toml");
    fs::write(&config, BASE_CONFIG).unwrap();
    let config_identity = FileIdentity::record(&config).unwrap();
    let expected: toml::Value = toml::from_str(BASE_CONFIG).unwrap();
    fs::write(
        frozen.join("expected.json"),
        serde_json::to_vec(&expected).unwrap(),
    )
    .unwrap();
    let peer = frozen.join("peer.cmd");
    fs::write(
        &peer,
        if startup_failure {
            "@echo off\r\nexit /b 5\r\n"
        } else {
            "@echo off\r\npowershell.exe -NoProfile -NonInteractive -File \"%~dp0peer.ps1\"\r\n"
        },
    )
    .unwrap();
    fs::write(frozen.join("peer.ps1"), r#"
function Send($value) { [Console]::WriteLine(($value | ConvertTo-Json -Depth 30 -Compress)) }
$expected = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'expected.json') -Raw | ConvertFrom-Json
while ($null -ne ($line = [Console]::In.ReadLine())) {
    $request = $line | ConvertFrom-Json
    switch ($request.method) {
        'initialize' { Send @{id=$request.id;result=@{userAgent='runner-test-peer'}} }
        'config/read' { Send @{id=$request.id;result=@{config=$expected;layers=@(@{name=@{type='user';file=([System.IO.Path]::Combine($env:CODEX_HOME, 'config.toml'));profile=$null};config=$expected})}} }
        'thread/start' { Send @{id=$request.id;result=@{thread=@{id='runner-thread'}}} }
        'turn/start' {
            Send @{method='item/started';params=@{threadId='runner-thread';turnId='runner-turn';item=@{id='tool-1';type='commandExecution'}}}
            if ($env:REPO_BENCHMARK_TEST_HANG -eq 'true') { Start-Sleep -Seconds 30 }
            Send @{method='item/completed';params=@{threadId='runner-thread';turnId='runner-turn';item=@{id='tool-1';type='commandExecution';status='completed';exitCode=0}}}
            Send @{method='turn/completed';params=@{threadId='runner-thread';turn=@{id='runner-turn';status='completed'}}}
            Send @{id=$request.id;result=@{turn=@{id='runner-turn'}}}
        }
    }
}
"#).unwrap();
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let analyzer_root = frozen.join("analyzer");
    fs::create_dir(&analyzer_root).unwrap();
    let analyzer_files = [
        "kd4_turn_latency_audit.py",
        "kd4_timing_analysis.py",
        "kd4_first_useful_action_analysis.py",
        "rollout_snapshot.py",
    ]
    .into_iter()
    .map(|name| {
        let target = analyzer_root.join(name);
        fs::copy(repo.join("scripts").join(name), &target).unwrap();
        FileIdentity::record(&target).unwrap()
    })
    .collect::<Vec<_>>();
    let python = FileIdentity::record(&which::which("python").unwrap()).unwrap();
    let environment = Environment {
        variables: std::env::vars().collect(),
        tools: BTreeMap::from([(
            "python".into(),
            ToolIdentity {
                executable: python,
                version: "integration-test interpreter".into(),
            },
        )]),
        rust_toolchain: "not-built-test-peer".into(),
    };
    // execute_one is below preparation validation: these identities deliberately
    // describe a test peer, and are never passed off as production build proof.
    let source = SourceIdentity {
        origin: repo.clone(),
        selection: "test-peer".into(),
        revision: "test-peer".into(),
        tree: "test-peer".into(),
        checkout: frozen.clone(),
        upstream: false,
    };
    let build = BuildIdentity {
        revision: "test-peer".into(),
        source: frozen.clone(),
        target_directory: frozen.clone(),
        settings: BTreeMap::new(),
        lockfile: config_identity.clone(),
        cargo_config: None,
        v8_artifacts: None,
        executables: BTreeMap::from([(
            "codex-app-server".into(),
            FileIdentity::record(&peer).unwrap(),
        )]),
        log: frozen.join("unused-build.log"),
        cache_key: "test-peer-no-build".into(),
        build_elapsed_ms: 0,
        cache_reuse_elapsed_ms: None,
    };
    Prepared {
        schema_version: MANIFEST_VERSION,
        id: "runner-integration".into(),
        directory: directory.clone(),
        repo,
        mode: Mode::Fast,
        schedule: schedule(Mode::Fast),
        workspace,
        additional_roots: vec![],
        runs_directory: directory.join("runs"),
        import_directory: directory.join("accepted"),
        fork: source.clone(),
        reference: source,
        builds: Variant::ALL
            .into_iter()
            .map(|variant| (variant, build.clone()))
            .collect(),
        harness: config_identity.clone(),
        harness_sources: config_identity.clone(),
        environment,
        base_config: config_identity.clone(),
        features: vec![],
        feature_inventory: config_identity,
        overrides: Variant::ALL
            .into_iter()
            .map(|variant| (variant, vec![]))
            .collect(),
        fixtures: BTreeMap::from([(
            "scripted".into(),
            FrozenFixture {
                sha256: hash_tree(&snapshot).unwrap(),
                snapshot: snapshot.clone(),
                descriptor: None,
            },
        )]),
        shared_inputs: snapshot.clone(),
        shared_sha256: hash_tree(&snapshot).unwrap(),
        analyzer: analyzer_files[0].clone(),
        analyzer_files,
        preparation_ms: 0,
        budgets: json!({}),
    }
}

fn attempt(prepared: &Prepared, live: bool, name: &str) -> Attempt {
    let directory = prepared.runs_directory.join(name);
    Attempt {
        scheduled: ScheduledAttempt {
            id: name.into(),
            segment: if live {
                Segment::RealModel
            } else {
                Segment::Scripted
            },
            workload: if live {
                LiveTask::RustBugfix.id()
            } else {
                "direct_tools"
            }
            .into(),
            variant: Variant::Reference,
            cluster: 0,
            repetition: 0,
            warmup: false,
        },
        status: "not_started".into(),
        reason: None,
        started_unix_ms: None,
        native: None,
        verifier: None,
        diagnostics: None,
        outside_execution_ms: BTreeMap::new(),
        final_workspace_sha256: None,
        evidence_directory: directory,
        rerun_command: format!("just repo-benchmark rerun --attempt {name}"),
    }
}

fn add_live_fixture(prepared: &mut Prepared, with_verifier: bool) {
    let snapshot = prepared.directory.join("fixtures/rust_bugfix");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(
        snapshot.join("AGENTS.md"),
        "Run the requested focused task.\n",
    )
    .unwrap();
    let descriptor = if with_verifier {
        reset_workspace(&prepared.directory, &prepared.workspace, &snapshot).unwrap();
        let protected = prepared.directory.join("protected/rust_bugfix");
        fs::create_dir_all(&protected).unwrap();
        let descriptor =
            prepare_fixture(LiveTask::RustBugfix, &prepared.workspace, &protected).unwrap();
        copy_tree(&prepared.workspace, &snapshot).unwrap();
        Some(descriptor)
    } else {
        None
    };
    prepared.fixtures.insert(
        LiveTask::RustBugfix.id().into(),
        FrozenFixture {
            sha256: hash_tree(&snapshot).unwrap(),
            snapshot,
            descriptor,
        },
    );
}

#[test]
fn startup_failure_survives_runner_audit_and_persistence_without_scripted_tokens() {
    let temp = tempfile::tempdir().unwrap();
    let prepared = prepared_peer(temp.path(), true);
    let mut attempt = attempt(&prepared, false, "startup-failure");
    execute_one(&prepared, &mut attempt, 10000).unwrap();
    assert_eq!(attempt.status, "setup_failed");
    assert_eq!(attempt.native.as_ref().unwrap().completed_turns, 0);
    assert!(attempt.native.as_ref().unwrap().rollout_paths.is_empty());
    let diagnostics = attempt.diagnostics.as_ref().unwrap();
    assert_eq!(diagnostics.status, "available", "{:?}", diagnostics.error);
    let report = &diagnostics.reports[0];
    assert_eq!(report["tokenAnalysisEnabled"], false);
    let tokens = report["runnerDiagnostics"]["tokens"].as_object().unwrap();
    assert_eq!(tokens["enabled"], false);
    assert_eq!(tokens["available"], false);
    assert_eq!(tokens["complete"], false);
    for (field, value) in tokens {
        if !["enabled", "available", "complete"].contains(&field.as_str()) {
            assert!(
                value.is_null(),
                "scripted token metric was computed: {field}={value}"
            );
        }
    }
    assert!(report["runnerDiagnostics"]["nativeProviderUsage"].is_null());
    assert!(
        report["runnerDiagnostics"]["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "setup_failure")
    );
    let saved: Attempt = read_json(&attempt.evidence_directory.join("attempt.json")).unwrap();
    assert_eq!(saved.status, "setup_failed");
    assert_eq!(
        saved.diagnostics.unwrap().reports[0]["tokenAnalysisEnabled"],
        false
    );
    let raw: Value =
        serde_json::from_slice(&fs::read(&attempt.native.as_ref().unwrap().evidence_path).unwrap())
            .unwrap();
    assert_eq!(raw["failure"]["kind"], "setup_failure");
    assert_eq!(raw["completedTurns"], 0);
}

#[test]
fn completed_native_turn_does_not_override_independent_incorrect_result() {
    let temp = tempfile::tempdir().unwrap();
    let mut prepared = prepared_peer(temp.path(), false);
    add_live_fixture(&mut prepared, true);
    let mut attempt = attempt(&prepared, true, "false-success");
    execute_one(&prepared, &mut attempt, 10000).unwrap();
    assert_eq!(
        attempt.native.as_ref().unwrap().status,
        "completed",
        "{:?}",
        attempt.reason
    );
    assert_eq!(attempt.native.as_ref().unwrap().tool_executions, 1);
    assert_eq!(attempt.native.as_ref().unwrap().completed_turns, 1);
    assert_eq!(attempt.status, "incorrect");
    assert_eq!(
        attempt.verifier.as_ref().unwrap().status,
        VerificationStatus::Incorrect
    );
    assert!(
        fs::read_to_string(&attempt.verifier.as_ref().unwrap().stdout_path)
            .unwrap()
            .contains("requested source change was not made")
    );
    let saved: Attempt = read_json(&attempt.evidence_directory.join("attempt.json")).unwrap();
    assert_eq!(saved.status, "incorrect");
    let report = &saved.diagnostics.as_ref().unwrap().reports[0];
    assert_eq!(report["tokenAnalysisEnabled"], true);
    assert!(
        report["runnerDiagnostics"]["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "verification_incorrect")
    );
}

#[test]
fn analyzer_failure_keeps_underlying_completed_native_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let mut prepared = prepared_peer(temp.path(), false);
    add_live_fixture(&mut prepared, false);
    fs::write(
        &prepared.analyzer.path,
        "raise RuntimeError('deliberate analyzer failure')\n",
    )
    .unwrap();
    prepared.analyzer = FileIdentity::record(&prepared.analyzer.path).unwrap();
    prepared.analyzer_files[0] = prepared.analyzer.clone();
    let mut attempt = attempt(&prepared, true, "analysis-failure");
    execute_one(&prepared, &mut attempt, 10000).unwrap();
    assert_eq!(attempt.status, "completed", "{:?}", attempt.reason);
    assert_eq!(attempt.diagnostics.as_ref().unwrap().status, "failed");
    assert!(
        fs::read_to_string(&attempt.diagnostics.as_ref().unwrap().stderr_paths[0])
            .unwrap()
            .contains("deliberate analyzer failure")
    );
    let saved: Attempt = read_json(&attempt.evidence_directory.join("attempt.json")).unwrap();
    assert_eq!(saved.status, "completed");
    assert_eq!(saved.native.as_ref().unwrap().completed_turns, 1);
    assert_eq!(saved.diagnostics.unwrap().status, "failed");
    let raw: Value =
        serde_json::from_slice(&fs::read(&saved.native.unwrap().evidence_path).unwrap()).unwrap();
    assert_eq!(raw["status"], "completed");
    assert_eq!(raw["toolExecutions"], 1);
}

#[test]
fn native_timeout_remains_visible_when_the_final_workspace_is_incorrect() {
    let temp = tempfile::tempdir().unwrap();
    let mut prepared = prepared_peer(temp.path(), false);
    prepared
        .environment
        .variables
        .insert("REPO_BENCHMARK_TEST_HANG".into(), "true".into());
    add_live_fixture(&mut prepared, true);
    let mut attempt = attempt(&prepared, true, "timeout-before-change");
    execute_one(&prepared, &mut attempt, 5000).unwrap();
    let native = attempt.native.as_ref().unwrap();
    assert_eq!(native.status, "timeout", "{:?}", native.failure);
    assert_eq!(native.failure.as_ref().unwrap().kind, "attempt_timeout");
    assert_eq!(native.completed_turns, 0);
    assert!(
        native
            .events
            .iter()
            .any(|event| event["message"]["method"] == "item/started")
    );
    assert!(
        !native
            .events
            .iter()
            .any(|event| event["message"]["method"] == "turn/completed")
    );
    assert_eq!(
        attempt.verifier.as_ref().unwrap().status,
        VerificationStatus::Incorrect
    );
    assert_eq!(attempt.status, "timeout");
    assert!(attempt.reason.as_ref().unwrap().contains("attempt_timeout"));
    let saved: Attempt = read_json(&attempt.evidence_directory.join("attempt.json")).unwrap();
    assert_eq!(saved.status, "timeout");
    assert_eq!(
        saved.verifier.as_ref().unwrap().status,
        VerificationStatus::Incorrect
    );
    let failures = saved.diagnostics.as_ref().unwrap().reports[0]["runnerDiagnostics"]["failures"]
        .as_array()
        .unwrap();
    assert!(
        failures
            .iter()
            .any(|failure| failure["kind"] == "attempt_timeout")
    );
    assert!(
        failures
            .iter()
            .any(|failure| failure["kind"] == "verification_incorrect")
    );
}

#[test]
fn analysis_only_uses_original_evidence_and_preserves_original_results() {
    let temp = tempfile::tempdir().unwrap();
    let prepared = prepared_peer(temp.path(), true);
    let mut attempt = attempt(&prepared, false, "original-attempt");
    attempt.scheduled = prepared
        .schedule
        .iter()
        .find(|scheduled| {
            scheduled.segment == Segment::Scripted
                && scheduled.workload == "direct_tools"
                && scheduled.variant == Variant::Reference
        })
        .unwrap()
        .clone();
    execute_one(&prepared, &mut attempt, 10000).unwrap();
    let native_path = attempt.native.as_ref().unwrap().evidence_path.clone();
    let original_native = fs::read(&native_path).unwrap();
    let original_attempt_path = attempt.evidence_directory.join("attempt.json");
    let original_attempt = fs::read(&original_attempt_path).unwrap();
    let original_audit_path = attempt.diagnostics.as_ref().unwrap().stdout_paths[0].clone();
    let original_audit = fs::read(&original_audit_path).unwrap();
    let manifest = prepared.directory.join("prepared.json");
    write_json(&manifest, &prepared).unwrap();
    let original_path = prepared.runs_directory.join("original-result.json");
    let original = RunResult {
        schema_version: 1,
        id: "original".into(),
        prepared_manifest: manifest.clone(),
        prepared_manifest_sha256: hash_file(&manifest).unwrap(),
        directory: prepared.runs_directory.clone(),
        mode: Mode::Fast,
        original_run: None,
        attempts: vec![attempt],
        scripted_execution_ms: 1,
        real_model_execution_ms: 0,
        finished: true,
    };
    write_json(&original_path, &original).unwrap();
    let original_bytes = fs::read(&original_path).unwrap();
    let rerun_path = analysis_only(&original_path).unwrap();
    assert_ne!(rerun_path, original_path);
    assert!(rerun_path.starts_with(&prepared.runs_directory));
    assert_eq!(fs::read(&original_path).unwrap(), original_bytes);
    assert_eq!(fs::read(&native_path).unwrap(), original_native);
    assert_eq!(fs::read(&original_attempt_path).unwrap(), original_attempt);
    assert_eq!(fs::read(&original_audit_path).unwrap(), original_audit);
    let rerun: RunResult = read_json(&rerun_path).unwrap();
    assert_eq!(rerun.original_run, Some(original_path));
    assert_eq!(
        rerun.attempts[0].native.as_ref().unwrap().evidence_path,
        native_path
    );
    assert_eq!(rerun.attempts[0].status, "setup_failed");
    assert_eq!(
        rerun.attempts[0].diagnostics.as_ref().unwrap().reports[0]["tokenAnalysisEnabled"],
        false
    );
    assert!(
        rerun.attempts[0].diagnostics.as_ref().unwrap().stdout_paths[0]
            .starts_with(&rerun.directory)
    );
    let report: Value = read_json(&rerun.directory.join("report.json")).unwrap();
    assert_eq!(report["completion"]["failed"], 1);
    assert_eq!(report["result"]["attempts"][0]["status"], "setup_failed");
    assert_eq!(
        report["result"]["attempts"][0]["diagnostics"]["reports"][0]["tokenAnalysisEnabled"],
        false
    );
    assert!(
        fs::read_to_string(rerun.directory.join("report.md"))
            .unwrap()
            .contains("setup_failed")
    );
}
