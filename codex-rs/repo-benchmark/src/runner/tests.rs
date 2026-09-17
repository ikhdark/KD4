//! Runner integration uses a local protocol peer, never a provider/model call.
#![cfg(windows)]

use super::*;
use crate::prepare::FrozenFixture;
use crate::prepare::MANIFEST_VERSION;
use crate::prepare::SourceIdentity;
use crate::prepare::builds::BuildIdentity;
use crate::prepare::environment::BASE_CONFIG;
use crate::prepare::environment::Environment;
use crate::prepare::environment::ToolIdentity;
use crate::prepare::provenance::FileIdentity;
use crate::prepare::provenance::hash_tree;
use crate::prepare::provenance::read_json;
use crate::schedule::Mode;
use crate::schedule::Variant;
use crate::schedule::schedule;
use crate::workloads::LiveTask;
use crate::workloads::prepare_fixture;
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
        fork_only_on: false,
        schedule: schedule(Mode::Fast, false),
        workspace,
        workspace_lock: directory.join("workspace.lock"),
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
        project_config_comparison: None,
        features: vec![],
        feature_inventory: config_identity,
        overrides: Variant::ALL
            .into_iter()
            .map(|variant| (variant, vec![]))
            .collect(),
        fixtures: schedule(Mode::Fast, false)
            .into_iter()
            .map(|scheduled| {
                let name = match scheduled.segment {
                    Segment::Scripted => "scripted".to_string(),
                    Segment::RealModel => scheduled.workload,
                };
                (
                    name,
                    FrozenFixture {
                        sha256: hash_tree(&snapshot).unwrap(),
                        snapshot: snapshot.clone(),
                        descriptor: None,
                    },
                )
            })
            .collect(),
        shared_inputs: snapshot.clone(),
        shared_sha256: hash_tree(&snapshot).unwrap(),
        analyzer: analyzer_files[0].clone(),
        analyzer_files,
        preparation_ms: 0,
        budgets: json!({"scriptedMs":1_800_000,"realModelMs":1_800_000,"attemptMs":600_000}),
    }
}

#[test]
fn fork_only_on_default_preserves_selection_and_unrun_pairs_in_reports_and_reanalysis() {
    let temp = tempfile::tempdir().unwrap();
    let mut prepared = prepared_peer(temp.path(), false);

    let options = crate::cli::parse(vec![]).unwrap();
    prepared.fork_only_on = options.fork_only_on;
    prepared.schedule = schedule(options.mode, options.fork_only_on);
    prepared.builds.remove(&Variant::ForkOff);
    prepared.overrides.remove(&Variant::ForkOff);
    prepared.features = vec![
        json!({"id":"runtime", "config_keys":["features.kd4_runtime"], "benchmark_on":true, "benchmark_control":{"kind":"runtime"}}),
    ];
    prepared.overrides.insert(
        Variant::ForkOn,
        crate::prepare::feature_overrides(&prepared.features, true).unwrap(),
    );
    let project = temp.path().join("project");
    fs::create_dir_all(project.join(".codex")).unwrap();
    fs::write(project.join(".codex/config.toml"), BASE_CONFIG).unwrap();
    prepared.project_config_comparison =
        crate::prepare::environment::ProjectConfigComparison::capture(
            &project,
            BASE_CONFIG,
            &prepared.overrides,
        )
        .unwrap();
    let manifest = prepared.directory.join("prepared.json");
    write_json(&manifest, &prepared).unwrap();
    let prepared = Prepared::load(&manifest).unwrap();
    let attempts: Vec<_> = prepared
        .schedule
        .iter()
        .map(|scheduled| {
            let mut value = attempt(
                &prepared,
                scheduled.segment == Segment::RealModel,
                &scheduled.id,
            );
            value.scheduled = scheduled.clone();
            value
        })
        .collect();
    let result = RunResult {
        schema_version: 1,
        id: "only-on".into(),
        prepared_manifest: manifest.clone(),
        prepared_manifest_sha256: hash_file(&manifest).unwrap(),
        directory: prepared.runs_directory.clone(),
        mode: options.mode,
        original_run: None,
        attempts,
        scripted_execution_ms: 0,
        real_model_execution_ms: 0,
        finished: true,
    };
    let path = result.directory.join("result.json");
    fs::create_dir_all(&result.directory).unwrap();
    write_json(&path, &result).unwrap();
    crate::reports::write(&prepared, &result).unwrap();
    for report_path in [
        result.directory.join("report.json"),
        analysis_only(&path).unwrap().with_file_name("report.json"),
    ] {
        let report: Value = read_json(&report_path).unwrap();
        assert_eq!(report["completion"]["scheduled"], 86);
        assert_eq!(report["completion"]["completed"], 0);
        assert_eq!(report["completion"]["unrun"], 86);
        assert_eq!(report["prepared"]["forkOnlyOn"], true);
        let comparisons = report["comparisons"].as_array().unwrap();
        assert!(!comparisons.is_empty());
        assert!(comparisons.iter().all(|c| c["kind"] == "overall"
            && c["baseline"] == "reference"
            && c["candidate"] == "fork_on"));
        let elapsed = comparisons
            .iter()
            .find(|c| c["workload"] == "long_history_initial" && c["metric"] == "elapsed_ms")
            .unwrap();
        assert!(elapsed["pairs"].as_array().unwrap().is_empty());
        assert_eq!(elapsed["scheduledPairSlots"], 3);
        assert_eq!(elapsed["excludedPairRate"], 1.0);
        assert_eq!(
            report["featureCoverage"][0]["configuredByVariant"]["fork_on"],
            true
        );
        assert_eq!(
            report["featureCoverage"][0]["ablationStatus"],
            "not_ablated"
        );
        for mapping in [
            &report["prepared"]["builds"],
            &report["prepared"]["overrides"],
            &report["prepared"]["projectConfigComparison"]["byVariant"],
            &report["featureCoverage"][0]["configuredByVariant"],
        ] {
            assert!(mapping.get("fork_off").is_none());
        }
        let markdown = fs::read_to_string(report_path.with_file_name("report.md")).unwrap();
        assert!(markdown.contains("Configured fork_on / reference"));
        assert!(!markdown.contains("fork_off"));
    }
    assert!(
        !fs::read_dir(&prepared.runs_directory)
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("fork_off"))
    );
}

#[test]
fn report_write_exposes_ablation_scope_verification_and_incomplete_pair_rates() {
    let temp = tempfile::tempdir().unwrap();
    let mut prepared = prepared_peer(temp.path(), false);
    prepared.features = vec![
        json!({"id":"runtime", "config_keys":["features.kd4_runtime"], "benchmark_on":true, "benchmark_control":{"kind":"runtime"}, "runtime_verification":{"kind":"contract_test", "path":"core/tests.rs", "symbol":"runtime_contract"}}),
        json!({"id":"preflight", "config_keys":["features.kd4_runtime"], "benchmark_on":true, "benchmark_control":{"kind":"runtime"}}),
        json!({"id":"disabled", "config_keys":["features.disabled"], "benchmark_on":false, "benchmark_control":{"kind":"runtime"}}),
        json!({"id":"fixed", "config_keys":[], "benchmark_control":{"kind":"lacking_off_state", "reason":"present in both fork builds"}}),
        json!({"id":"workflow", "config_keys":[], "benchmark_control":{"kind":"lacking_off_state", "reason":"repository workflow"}}),
    ];
    for (variant, enabled) in [(Variant::ForkOff, false), (Variant::ForkOn, true)] {
        prepared.overrides.insert(
            variant,
            crate::prepare::feature_overrides(&prepared.features, enabled).unwrap(),
        );
    }
    prepared.features.push(json!({"id":"unknown", "config_keys":["features.missing"], "benchmark_control":{"kind":"runtime"}}));
    let project = temp.path().join("project");
    fs::create_dir_all(project.join(".codex")).unwrap();
    fs::write(
        project.join(".codex/config.toml"),
        "approval_policy = 'never'\nallow_login_shell = false\n[features]\nkd4_runtime = true\n",
    )
    .unwrap();
    prepared.project_config_comparison =
        crate::prepare::environment::ProjectConfigComparison::capture(
            &project,
            BASE_CONFIG,
            &prepared.overrides,
        )
        .unwrap();
    let code_mode_host = prepared.harness.clone();
    prepared
        .builds
        .get_mut(&Variant::ForkOn)
        .unwrap()
        .executables
        .insert("codex-code-mode-host".into(), code_mode_host);
    let mut attempt = attempt(&prepared, false, "unrun");
    attempt.scheduled = prepared.schedule[0].clone();
    prepared.schedule = vec![attempt.scheduled.clone()];
    let manifest = prepared.directory.join("prepared.json");
    write_json(&manifest, &prepared).unwrap();
    fs::create_dir_all(&prepared.runs_directory).unwrap();
    let result = RunResult {
        schema_version: 1,
        id: "coverage-report".into(),
        prepared_manifest: manifest.clone(),
        prepared_manifest_sha256: hash_file(&manifest).unwrap(),
        directory: prepared.runs_directory.clone(),
        mode: Mode::Fast,
        original_run: None,
        attempts: vec![attempt],
        scripted_execution_ms: 0,
        real_model_execution_ms: 0,
        finished: true,
    };
    write_json(&result.directory.join("result.json"), &result).unwrap();
    crate::reports::write(&prepared, &result).unwrap();
    let report: Value = read_json(&result.directory.join("report.json")).unwrap();
    assert_eq!(
        report["ablationCounts"],
        json!({"runtime_changed":2, "runtime_unchanged":1, "not_ablated":2, "configuration_unavailable":1})
    );
    assert_eq!(
        report["coupledControls"],
        json!({"features.kd4_runtime":["runtime", "preflight"]})
    );
    let features = report["featureCoverage"].as_array().unwrap();
    assert_eq!(
        report["prepared"]["projectConfigComparison"]["byVariant"]["fork_off"]["changed"],
        json!(["features.kd4_runtime"])
    );
    assert_eq!(
        report["prepared"]["projectConfigComparison"]["byVariant"]["fork_on"]["changed"],
        json!([])
    );
    assert_eq!(
        features[0]["declaredVerification"]["symbol"],
        "runtime_contract"
    );
    assert_eq!(
        features[0]["configuredSettingsByVariant"]["fork_off"]["features.kd4_runtime"],
        false
    );
    assert_eq!(
        features[0]["configuredSettingsByVariant"]["fork_on"]["features.kd4_runtime"],
        true
    );
    assert!(
        features
            .iter()
            .all(|feature| feature["exercised"].is_null())
    );
    assert_eq!(features[3]["ablationStatus"], "not_ablated");
    assert_eq!(features[5]["ablationStatus"], "configuration_unavailable");
    let markdown = fs::read_to_string(result.directory.join("report.md")).unwrap();
    for expected in [
        "2 change runtime settings",
        "1 keep identical runtime settings",
        "2 are not ablated",
        "1 have unavailable configuration",
        "Shared control `features.kd4_runtime`",
        "core/tests.rs::runtime_contract",
        "not controlled in either fork arm",
        "100.0% incomplete",
        "Excluded pairs",
        "100.0%",
        "linear interpolation at (n-1)*q",
        "## Configuration scope",
        "allow_login_shell",
        "excludes home configuration",
        "Code-mode host availability differs across variants",
        "fork_on: code-mode host present",
        "reference: code-mode host absent",
    ] {
        assert!(
            markdown.contains(expected),
            "missing {expected}: {markdown}"
        );
    }
    let represented: Vec<_> = report["comparisons"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["scheduledPairSlots"] == 1)
        .collect();
    assert_eq!(represented.len(), 2);
    assert!(represented.iter().all(|row| row["excludedPairRate"] == 1.0));
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
        evidence_files: vec![],
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
        reset_workspace(
            &prepared.directory,
            &prepared.workspace,
            &snapshot,
            &hash_tree(&snapshot).unwrap(),
        )
        .unwrap();
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
    let native = attempt.native.as_ref().unwrap();
    let raw: Value = serde_json::from_slice(&fs::read(&native.evidence_path).unwrap()).unwrap();
    assert!(
        raw.get("verifier").is_none(),
        "verification must not rewrite native evidence"
    );
    assert_eq!(raw, serde_json::to_value(native).unwrap());
    let analysis_input: Value = serde_json::from_slice(
        &fs::read(attempt.evidence_directory.join("analysis-input.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(analysis_input["verifier"]["status"], "incorrect");
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
fn out_of_scope_edit_survives_runner_verification_and_persistence() {
    let temp = tempfile::tempdir().unwrap();
    let mut prepared = prepared_peer(temp.path(), false);
    add_live_fixture(&mut prepared, true);
    let peer = prepared.directory.join("frozen/peer.ps1");
    let source = fs::read_to_string(&peer).unwrap();
    let instructions = prepared
        .workspace
        .join("AGENTS.md")
        .display()
        .to_string()
        .replace('\'', "''");
    fs::write(&peer, source.replace("'turn/start' {", &format!(
        "'turn/start' {{\n[System.IO.File]::WriteAllText('{instructions}', 'changed instructions')"
    ))).unwrap();
    let mut attempt = attempt(&prepared, true, "scope-violation");
    execute_one(&prepared, &mut attempt, 10000).unwrap();
    assert_eq!(attempt.native.as_ref().unwrap().status, "completed");
    assert_eq!(attempt.status, "scope_violation");
    assert_eq!(
        attempt.verifier.as_ref().unwrap().status,
        VerificationStatus::ScopeViolation
    );
    assert!(
        attempt
            .verifier
            .as_ref()
            .unwrap()
            .detail
            .contains("AGENTS.md")
    );
    let saved: Attempt = read_json(&attempt.evidence_directory.join("attempt.json")).unwrap();
    assert_eq!(saved.status, "scope_violation");
    assert!(
        saved.diagnostics.as_ref().unwrap().reports[0]["runnerDiagnostics"]["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["kind"] == "verification_scope_violation")
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
    assert_eq!(rerun.original_run, Some(original_path.clone()));
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
    fs::write(&native_path, b"{}").unwrap();
    assert!(
        analysis_only(&original_path)
            .unwrap_err()
            .to_string()
            .contains("changed prepared artifact")
    );
    assert!(crate::reports::import(&rerun_path).is_err());
    assert_eq!(fs::read(&original_path).unwrap(), original_bytes);
}

#[test]
fn reports_compare_versioned_behavior_from_all_frozen_audit_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let mut prepared = prepared_peer(temp.path(), false);
    prepared.schedule.retain(|item| {
        item.segment == Segment::Scripted
            && item.workload == "direct_tools"
            && item.cluster == 0
            && !item.warmup
    });
    assert_eq!(prepared.schedule.len(), 3);
    fs::create_dir_all(&prepared.runs_directory).unwrap();
    let manifest = prepared.directory.join("prepared.json");
    write_json(&manifest, &prepared).unwrap();
    let mut attempts = Vec::new();
    for scheduled in &prepared.schedule {
        let mut item = attempt(&prepared, false, &scheduled.id);
        item.scheduled = scheduled.clone();
        item.status = "completed".into();
        fs::create_dir_all(&item.evidence_directory).unwrap();
        let searches = match scheduled.variant {
            Variant::Reference => 3,
            Variant::ForkOff => 4,
            Variant::ForkOn => 1,
        };
        let capture = item.evidence_directory.join("requests.jsonl");
        fs::write(
            &capture,
            "{\"request\":{\"input\":\"x\"}}\n".repeat(searches),
        )
        .unwrap();
        let mut paths = Vec::new();
        for (session, count) in [searches, 2].into_iter().enumerate() {
            let path = item
                .evidence_directory
                .join(format!("rollout-{session}.jsonl"));
            let record = |kind, payload| {
                json!({"type":kind,"timestamp":"2026-08-17T00:00:01Z","payload":payload})
                    .to_string()
            };
            let mut lines = vec![
                record("session_meta", json!({"cwd": prepared.workspace})),
                record(
                    "event_msg",
                    json!({"type":"task_started","turn_id":format!("turn-{session}")}),
                ),
            ];
            for call in 0..count {
                lines.push(record("response_item", json!({"type":"function_call","name":"exec_command","call_id":format!("call-{call}"),"arguments":{"cmd":"rg needle src/widget.rs"}})));
                lines.push(record("response_item", json!({"type":"function_call_output","call_id":format!("call-{call}"),"output":""})));
            }
            let terminal = record(
                "event_msg",
                json!({
                    "type":"task_complete", "turn_id":format!("turn-{session}"),
                    "timing": {
                        "schemaVersion":25, "profileValid":true, "classificationComplete":true,
                        "counters": {
                            "saturationCount":0, "executedValidationCount":1,
                            "executedValidationDurationNs":count * 100 + session,
                            "suppressedValidationOutputCount":count + 1,
                            "noProgressDirectiveCount":count, "provenLoopActivationCount":session,
                            "planningGenerationCount":count + 2, "planRevisionGenerationCount":session,
                            "planningFixedPointIterationCount":count + 3, "approvalWaitCount":count,
                            "permissionWaitCount":0, "userInputWaitCount":session, "mcpElicitationWaitCount":2,
                            "toolOutputCanonicalTokenCount":count * 1000,
                            "toolOutputModelTokenCount":count * 100,
                            "toolOutputRecoveryCallCount":count,
                            "toolOutputRecoveryRetruncationCount":session,
                            "toolOutputArtifactCreationCount":count,
                            "toolOutputArtifactReuseCount":session,
                            "toolOutputOmittedSectionCount":count * 10,
                            "attributableRecoveryGenerationCount":session
                        }
                    }
                }),
            );
            // Replaying a terminal notification must not add another turn's counters.
            lines.extend([terminal.clone(), terminal]);
            fs::write(&path, lines.join("\n") + "\n").unwrap();
            paths.push(path);
        }
        let native: crate::native::NativeAttemptEvidence = serde_json::from_value(json!({
            "schemaVersion":1,"attemptId":scheduled.id,"status":"completed","elapsedMs":100,"cleanupMs":0,
            "threadId":"synthetic-report-test","completedTurns":2,"toolExecutions":searches + 2,
            "failure":null,"effectiveConfig":{},"events":[{"message":{"method":"turn/completed","params":{"turn":{"id":"dispatch-turn","status":"completed","timing":{
                "schemaVersion":25,"profileValid":true,"classificationComplete":true,
                "counters":{"modelRequestCount":0},"modelRequests":[],
                "toolCalls":[{"callId":"dispatch","retryCount":searches,"reentryCount":2}]
            }}}}}],"stdoutPaths":[],"stderrPaths":[],
            "rolloutPaths":paths,"providerRequestsPath":capture,"adaptations":[],
            "evidencePath":item.evidence_directory.join("native-evidence.json")
        })).unwrap();
        write_json(&native.evidence_path, &native).unwrap();
        let diagnostics = crate::diagnostics::analyze(
            &prepared.environment.tools["python"].executable.path,
            &prepared.analyzer.path,
            &prepared.analyzer_files,
            &native,
            &item.evidence_directory.join("audit"),
            &prepared.workspace,
            false,
            None,
        );
        assert_eq!(diagnostics.status, "available", "{:?}", diagnostics.error);
        assert_eq!(diagnostics.reports.len(), 2);
        item.native = Some(native);
        item.diagnostics = Some(diagnostics);
        attempts.push(item);
    }
    let result = RunResult {
        schema_version: 1,
        id: "behavior-report".into(),
        prepared_manifest: manifest.clone(),
        prepared_manifest_sha256: hash_file(&manifest).unwrap(),
        directory: prepared.runs_directory.clone(),
        mode: prepared.mode,
        original_run: None,
        attempts,
        scripted_execution_ms: 300,
        real_model_execution_ms: 0,
        finished: true,
    };
    write_json(&result.directory.join("result.json"), &result).unwrap();
    crate::reports::write(&prepared, &result).unwrap();
    let report: Value = read_json(&result.directory.join("report.json")).unwrap();
    assert_eq!(report["behaviorSchemaVersion"], 2);
    assert_eq!(report["completion"]["completed"], 3);
    let comparisons = report["comparisons"].as_array().unwrap();
    let retries = comparisons
        .iter()
        .find(|value| value["metric"] == "tool_retries" && value["kind"] == "feature_effect")
        .unwrap();
    assert_eq!(retries["candidateDistribution"]["median"], 1.0);
    assert_eq!(retries["baselineDistribution"]["median"], 4.0);
    let reentries = comparisons
        .iter()
        .find(|value| value["metric"] == "tool_reentries" && value["kind"] == "feature_effect")
        .unwrap();
    assert_eq!(reentries["candidateDistribution"]["median"], 2.0);
    assert_eq!(reentries["baselineDistribution"]["median"], 2.0);
    assert!(
        fs::read_to_string(result.directory.join("report.md"))
            .unwrap()
            .contains("tool_retries (count)")
    );
    for (kind, expected) in [("drift", 1.0), ("feature_effect", -3.0), ("overall", -2.0)] {
        let comparison = comparisons
            .iter()
            .find(|value| value["metric"] == "discovery_searches" && value["kind"] == kind)
            .unwrap();
        assert_eq!(comparison["observed"]["medianDifference"], expected);
        assert_eq!(comparison["pairedObserved"]["medianDifference"], expected);
        assert_eq!(comparison["pairs"].as_array().unwrap().len(), 1);
        assert_eq!(
            comparison["intervalUnavailableReason"],
            "exploratory_behavior_counts"
        );
        assert!(comparison["bootstrap"].is_null());
    }
    let overall = comparisons
        .iter()
        .find(|value| value["metric"] == "discovery_searches" && value["kind"] == "overall")
        .unwrap();
    assert_eq!(overall["baselineDistribution"]["samples"][0]["value"], 5.0);
    assert_eq!(overall["candidateDistribution"]["samples"][0]["value"], 3.0);
    for (metric, baseline, candidate, unit) in [
        ("behavior_executed_validations", 2.0, 2.0, "count"),
        ("behavior_validation_duration_ns", 501.0, 301.0, "ns"),
        ("behavior_suppressed_validation_outputs", 7.0, 5.0, "count"),
        ("behavior_no_progress_directives", 5.0, 3.0, "count"),
        ("behavior_proven_loop_activations", 1.0, 1.0, "count"),
        ("behavior_planning_generations", 9.0, 7.0, "count"),
        ("behavior_plan_revision_generations", 1.0, 1.0, "count"),
        (
            "behavior_planning_fixed_point_iterations",
            11.0,
            9.0,
            "count",
        ),
        ("behavior_approval_waits", 5.0, 3.0, "count"),
        ("behavior_permission_waits", 0.0, 0.0, "count"),
        ("behavior_user_input_waits", 1.0, 1.0, "count"),
        ("behavior_mcp_elicitation_waits", 4.0, 4.0, "count"),
        (
            "behavior_tool_output_canonical_tokens",
            5000.0,
            3000.0,
            "tokens",
        ),
        ("behavior_tool_output_model_tokens", 500.0, 300.0, "tokens"),
        ("behavior_tool_output_recovery_calls", 5.0, 3.0, "count"),
        ("behavior_tool_output_artifact_creations", 5.0, 3.0, "count"),
        ("behavior_tool_output_artifact_reuses", 1.0, 1.0, "count"),
        ("behavior_tool_output_omitted_sections", 50.0, 30.0, "count"),
        ("behavior_recovery_generations", 1.0, 1.0, "count"),
        (
            "behavior_tool_output_recovery_retruncations",
            1.0,
            1.0,
            "count",
        ),
    ] {
        let comparison = comparisons
            .iter()
            .find(|value| value["metric"] == metric && value["kind"] == "overall")
            .unwrap();
        assert_eq!(comparison["unit"], unit, "{metric}");
        assert_eq!(
            comparison["baselineDistribution"]["samples"][0]["value"], baseline,
            "{metric}"
        );
        assert_eq!(
            comparison["candidateDistribution"]["samples"][0]["value"], candidate,
            "{metric}"
        );
        assert_eq!(
            comparison["pairedObserved"]["medianDifference"],
            candidate - baseline,
            "{metric}"
        );
        assert!(comparison["bootstrap"].is_null(), "{metric}");
    }
    assert_eq!(
        overall["pairs"][0]["baselineId"],
        overall["baselineDistribution"]["samples"][0]["id"]
    );
    let requests = comparisons
        .iter()
        .find(|value| value["metric"] == "scripted_request_count" && value["kind"] == "overall")
        .unwrap();
    assert_eq!(requests["baselineDistribution"]["median"], 3.0);
    assert_eq!(requests["candidateDistribution"]["median"], 1.0);
    assert_eq!(requests["pairedObserved"]["medianDifference"], -2.0);
    let bytes = comparisons
        .iter()
        .find(|value| {
            value["metric"] == "scripted_serialized_request_bytes" && value["kind"] == "overall"
        })
        .unwrap();
    assert_eq!(bytes["unit"], "bytes");
    assert_eq!(bytes["baselineDistribution"]["median"], 39.0);
    assert_eq!(bytes["candidateDistribution"]["median"], 13.0);
    assert_eq!(bytes["pairedObserved"]["medianDifference"], -26.0);
    assert!(
        !comparisons
            .iter()
            .any(|value| value["metric"] == "behavior_total_tokens"
                || value["metric"] == "behavior_model_retries")
    );
    let markdown = fs::read_to_string(result.directory.join("report.md")).unwrap();
    assert!(markdown.contains("discovery_searches (count)"));
    assert!(markdown.contains("Behavior schema 2"));
    assert!(markdown.contains("behavior_validation_duration_ns (ns)"));
    assert!(markdown.contains("\"behaviorMetrics\""));
    assert!(markdown.contains("scripted_serialized_request_bytes (bytes)"));
    let request_row = markdown
        .lines()
        .find(|line| line.contains("scripted_request_count (count)") && line.contains("overall"))
        .unwrap();
    assert!(
        request_row
            .ends_with("| unavailable | unavailable | suppressed (n<20) / suppressed (n<20) |"),
        "{request_row}"
    );
}

#[test]
fn workspace_lock_excludes_another_run_and_releases_on_owner_exit() {
    let temp = tempfile::tempdir().unwrap();
    let prepared = prepared_peer(temp.path(), true);
    let first = lock_workspace(&prepared).unwrap();
    assert!(
        lock_workspace(&prepared)
            .unwrap_err()
            .to_string()
            .contains("already in use")
    );
    drop(first);
    let second = lock_workspace(&prepared).unwrap();
    assert!(lock_workspace(&prepared).is_err());
    drop(second);
}

#[test]
fn evidence_binding_rejects_rollout_changes_and_missing_identities() {
    let temp = tempfile::tempdir().unwrap();
    let prepared = prepared_peer(temp.path(), true);
    let mut attempt = attempt(&prepared, false, "bound-evidence");
    execute_one(&prepared, &mut attempt, 10000).unwrap();
    let rollout = attempt.evidence_directory.join("rollout.jsonl");
    fs::write(&rollout, "{\"original\":true}\n").unwrap();
    let native = attempt.native.as_mut().unwrap();
    native.rollout_paths.push(rollout.clone());
    fs::write(&native.evidence_path, serde_json::to_vec(native).unwrap()).unwrap();
    freeze_attempt_evidence(&mut attempt).unwrap();
    verify_attempt_evidence(&attempt).unwrap();
    fs::write(&rollout, "{\"original\":false}\n").unwrap();
    assert!(
        verify_attempt_evidence(&attempt)
            .unwrap_err()
            .to_string()
            .contains("changed prepared artifact")
    );
    fs::write(&rollout, "{\"original\":true}\n").unwrap();
    attempt.evidence_files.clear();
    assert!(
        verify_attempt_evidence(&attempt)
            .unwrap_err()
            .to_string()
            .contains("no frozen identity")
    );
}

#[test]
fn per_attempt_checkpoints_recover_an_interrupted_run_without_rewriting_prior_traces() {
    let temp = tempfile::tempdir().unwrap();
    let prepared = prepared_peer(temp.path(), true);
    let mut first = attempt(&prepared, false, "first");
    first.status = "not_started".into();
    let mut second = attempt(&prepared, false, "second");
    second.status = "not_started".into();
    let directory = prepared.runs_directory.clone();
    fs::create_dir_all(&directory).unwrap();
    let result_path = directory.join("result.json");
    let initial = RunResult {
        schema_version: 1,
        id: "interrupted".into(),
        prepared_manifest: prepared.directory.join("prepared.json"),
        prepared_manifest_sha256: "test schedule".into(),
        directory,
        mode: Mode::Fast,
        original_run: None,
        attempts: vec![first.clone(), second.clone()],
        scripted_execution_ms: 0,
        real_model_execution_ms: 0,
        finished: false,
    };
    write_json(&result_path, &initial).unwrap();
    let frozen_bytes = fs::read(&result_path).unwrap();
    first.status = "setup_failed".into();
    first.reason = Some("confirmed startup failure".into());
    first.native = Some(
        serde_json::from_value(json!({
            "schemaVersion":1,"attemptId":"first","status":"setup_failed",
            "elapsedMs":123,"cleanupMs":1,"threadId":null,"completedTurns":0,
            "toolExecutions":0,"failure":{"kind":"setup_failure","message":"failed"},
            "effectiveConfig":{},"events":[{"message":"original event"}],
            "stdoutPaths":[],"stderrPaths":[],"rolloutPaths":[],
            "providerRequestsPath":null,"adaptations":[],"evidencePath":"raw.json"
        }))
        .unwrap(),
    );
    checkpoint(&first).unwrap();
    let first_bytes = fs::read(first.evidence_directory.join("attempt.json")).unwrap();
    second.status = "running".into();
    second.started_unix_ms = Some(999);
    checkpoint(&second).unwrap();
    assert_eq!(fs::read(&result_path).unwrap(), frozen_bytes);
    assert_eq!(
        fs::read(first.evidence_directory.join("attempt.json")).unwrap(),
        first_bytes
    );
    let recovered = RunResult::load(&result_path).unwrap();
    assert_eq!(recovered.attempts[0].status, "setup_failed");
    assert_eq!(
        recovered.attempts[0].native.as_ref().unwrap().events[0]["message"],
        "original event"
    );
    assert_eq!(recovered.scripted_execution_ms, 123);
    assert_eq!(recovered.real_model_execution_ms, 0);
    assert_eq!(recovered.attempts[1].status, "incomplete");
    assert_eq!(recovered.attempts[1].started_unix_ms, Some(999));
    assert!(!recovered.finished);
    // Checksum-valid evidence from another workload is still incompatible.
    first.scheduled.workload = "wrong_task".into();
    checkpoint(&first).unwrap();
    assert!(
        RunResult::load(&result_path)
            .unwrap_err()
            .to_string()
            .contains("frozen schedule")
    );
}

#[test]
fn final_source_evidence_preserves_changes_without_hashing_build_products() {
    let temp = tempfile::tempdir().unwrap();
    let snapshot = temp.path().join("snapshot");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("source.rs"), "original").unwrap();
    fs::write(snapshot.join("deleted.rs"), "delete me").unwrap();
    copy_tree(&snapshot, &workspace).unwrap();
    fs::write(workspace.join("source.rs"), "fixed").unwrap();
    fs::remove_file(workspace.join("deleted.rs")).unwrap();
    fs::write(workspace.join("new.rs"), "new behavior").unwrap();
    for directory in ["target", "node_modules", "__pycache__"] {
        fs::create_dir(workspace.join(directory)).unwrap();
        fs::write(workspace.join(directory).join("generated"), "build bytes").unwrap();
    }
    let evidence = temp.path().join("evidence");
    let digest = preserve_final_changes(&snapshot, &workspace, &evidence).unwrap();
    let changes: Value = read_json(&evidence.join("changes.json")).unwrap();
    assert_eq!(changes.as_array().unwrap().len(), 3);
    assert_eq!(
        fs::read_to_string(evidence.join("source.rs")).unwrap(),
        "fixed"
    );
    assert_eq!(
        fs::read_to_string(evidence.join("new.rs")).unwrap(),
        "new behavior"
    );
    assert!(
        changes
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == "deleted.rs" && row["afterSha256"].is_null())
    );
    assert!(!evidence.join("target").exists());
    fs::write(workspace.join("target/generated"), "different build bytes").unwrap();
    assert_eq!(source_tree_inventory(&workspace).unwrap().sha256, digest);
    fs::write(workspace.join("source.rs"), "regression").unwrap();
    assert_ne!(source_tree_inventory(&workspace).unwrap().sha256, digest);
}

#[test]
fn import_publishes_complete_reports_and_failed_copy_is_retryable() {
    let temp = tempfile::tempdir().unwrap();
    let prepared = prepared_peer(temp.path(), false);
    let manifest = prepared.directory.join("prepared.json");
    write_json(&manifest, &prepared).unwrap();
    let directory = prepared.runs_directory.join("atomic-import");
    fs::create_dir_all(&directory).unwrap();
    let result = RunResult {
        schema_version: 1,
        id: "atomic-import".into(),
        prepared_manifest: manifest.clone(),
        prepared_manifest_sha256: hash_file(&manifest).unwrap(),
        directory: directory.clone(),
        mode: prepared.mode,
        original_run: None,
        attempts: prepared
            .schedule
            .iter()
            .map(|scheduled| {
                let mut unrun = attempt(
                    &prepared,
                    scheduled.segment == Segment::RealModel,
                    &scheduled.id,
                );
                unrun.scheduled = scheduled.clone();
                unrun
            })
            .collect(),
        scripted_execution_ms: 0,
        real_model_execution_ms: 0,
        finished: true,
    };
    let result_path = directory.join("result.json");
    write_json(&result_path, &result).unwrap();
    crate::reports::write(&prepared, &result).unwrap();
    let names = [
        "result.json",
        "report.json",
        "report.md",
        "reports-manifest.json",
    ];
    let original: Vec<_> = names
        .iter()
        .map(|name| fs::read(directory.join(name)).unwrap())
        .collect();
    let destination = prepared.import_directory.join(&result.id);
    fs::create_dir_all(&destination).unwrap();
    assert!(
        crate::reports::import(&result_path)
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );
    assert_eq!(
        fs::read_dir(&destination).unwrap().count(),
        0,
        "existing empty import must remain untouched"
    );
    fs::remove_dir(&destination).unwrap();
    crate::reports::FAIL_IMPORT_AFTER_COPY.with(|remaining| remaining.set(Some(1)));
    assert!(
        crate::reports::import(&result_path)
            .unwrap_err()
            .to_string()
            .contains("injected report import copy failure")
    );
    assert!(
        !destination.exists(),
        "failed staging must not reserve the accepted run ID"
    );
    assert!(
        !fs::read_dir(&prepared.import_directory)
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".report-import-")),
        "failed staging must be removed"
    );
    for (name, bytes) in names.iter().zip(&original) {
        assert_eq!(
            &fs::read(directory.join(name)).unwrap(),
            bytes,
            "import must preserve source {name}"
        );
    }
    assert_eq!(crate::reports::import(&result_path).unwrap(), destination);
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 3);
    for name in &names[1..] {
        assert_eq!(
            fs::read(destination.join(name)).unwrap(),
            fs::read(directory.join(name)).unwrap()
        );
    }
    assert!(
        crate::reports::import(&result_path)
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );
}
