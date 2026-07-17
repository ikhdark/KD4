use super::*;
use crate::function_tool::FunctionCallError;
use crate::session::tests::make_session_and_context;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::TaskCompletionStatus;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
fn automatic_validation_command(long_running: bool) -> Vec<codex_verify_local::CommandArgV2> {
    let script = if long_running {
        "Start-Sleep -Seconds 30"
    } else {
        "exit 0"
    };
    [
        "powershell.exe",
        "-NonInteractive",
        "-NoLogo",
        "-Command",
        script,
    ]
        .into_iter()
        .map(codex_verify_local::CommandArgV2::text)
        .collect()
}

#[cfg(not(windows))]
fn automatic_validation_command(long_running: bool) -> Vec<codex_verify_local::CommandArgV2> {
    let script = if long_running { "sleep 30" } else { "exit 0" };
    ["sh", "-c", script]
        .into_iter()
        .map(codex_verify_local::CommandArgV2::text)
        .collect()
}

fn automatic_validation_plan(
    repo_root: &std::path::Path,
    changed_path: &str,
    long_running: bool,
) -> codex_verify_local::PlanEnvelopeV2 {
    let changed = RawPath::from_utf8(changed_path);
    let snapshot = RepositorySnapshot::from_explicit_paths(repo_root, [changed.clone()])
        .expect("explicit automatic validation snapshot");
    let mut plan = plan_verification(
        PlanRequest {
            mode: Some(PlanMode::Fast),
            changed: vec![changed.clone()],
            ..PlanRequest::default()
        },
        snapshot,
    );
    plan.verdict = None;
    plan.commands = vec![codex_verify_local::CommandSpecV2 {
        id: if long_running {
            "automatic-cancellation"
        } else {
            "automatic-success"
        }
        .to_string(),
        kind: "owner_test".to_string(),
        args: automatic_validation_command(long_running),
        cwd: RawPath::from_utf8("."),
        timeout_ms: 30_000,
        owner_packages: vec!["codex-core".to_string()],
        hash_paths: vec![changed],
        reason: "exercise Core's in-process automatic validation execution".to_string(),
    }];
    plan
}

#[tokio::test]
async fn automatic_validation_cancels_stale_generation_then_records_one_exact_fast_receipt() {
    const CHANGED_PATH: &str = "codex-rs/core/src/task_evidence.rs";
    let (session, mut turn) = make_session_and_context().await;
    turn.approval_policy
        .set(AskForApproval::Never)
        .expect("test approval policy");
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    #[allow(deprecated)]
    let repo_root = find_verify_local_repo_root(&turn.cwd).expect("KD4 repository root");
    let ledger = &session.services.task_evidence;
    ledger
        .seed_automatic_validation_mutation_for_test(&[CHANGED_PATH])
        .await;
    let stale = ledger
        .take_automatic_verify_plan_request()
        .await
        .expect("stale automatic claim");
    let stale_generation = stale.evidence_generation;
    let stale_claim_id = stale.claim_id.clone();
    let stale_run = tokio::spawn(run_automatic_verify_local_with_plan(
        Arc::clone(&session),
        Arc::clone(&step_context),
        Arc::clone(&tracker),
        stale,
        CancellationToken::new(),
        automatic_validation_plan(&repo_root, CHANGED_PATH, true),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ledger.validation_active_for_generation_for_test(stale_generation) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("automatic validation should become active");

    ledger
        .seed_automatic_validation_mutation_for_test(&[CHANGED_PATH])
        .await;
    tokio::time::timeout(Duration::from_secs(5), stale_run)
        .await
        .expect("stale automatic validation should cancel promptly")
        .expect("stale automatic validation task should join")
        .expect("cancelled verifier execution still returns its contract");
    let stale_receipts = ledger
        .automatic_validation_receipts_for_test(&stale_claim_id)
        .await;
    assert_eq!(stale_receipts.len(), 1);
    assert!(!stale_receipts[0].2);

    let current = ledger
        .take_automatic_verify_plan_request()
        .await
        .expect("current automatic claim");
    let current_claim_id = current.claim_id.clone();
    run_automatic_verify_local_with_plan(
        Arc::clone(&session),
        Arc::clone(&step_context),
        Arc::clone(&tracker),
        current,
        CancellationToken::new(),
        automatic_validation_plan(&repo_root, CHANGED_PATH, false),
    )
    .await
    .expect("current automatic validation");

    assert_eq!(
        ledger
            .automatic_validation_receipts_for_test(&current_claim_id)
            .await,
        vec![(
            "fast".to_string(),
            vec![CHANGED_PATH.to_string()],
            true,
        )]
    );
}

#[tokio::test]
async fn automatic_fast_pass_cannot_override_stronger_final_failure() {
    const CHANGED_PATH: &str = "codex-rs/core/src/task_evidence.rs";
    let (session, mut turn) = make_session_and_context().await;
    turn.approval_policy
        .set(AskForApproval::Never)
        .expect("test approval policy");
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    #[allow(deprecated)]
    let repo_root = find_verify_local_repo_root(&turn.cwd).expect("KD4 repository root");
    let ledger = &session.services.task_evidence;
    ledger
        .seed_automatic_validation_mutation_for_test(&[CHANGED_PATH])
        .await;
    let first = ledger
        .take_automatic_verify_plan_request()
        .await
        .expect("first automatic claim");
    run_automatic_verify_local_with_plan(
        Arc::clone(&session),
        Arc::clone(&step_context),
        Arc::clone(&tracker),
        first,
        CancellationToken::new(),
        automatic_validation_plan(&repo_root, CHANGED_PATH, false),
    )
    .await
    .expect("first automatic fast pass");

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
                &[PathBuf::from(CHANGED_PATH)],
                &[],
                Some(&json!({"verdict": "FAILED"})),
            )
            .await
    );
    let retry = ledger
        .take_automatic_verify_plan_request()
        .await
        .expect("automatic claim after final failure");
    let retry_claim_id = retry.claim_id.clone();
    run_automatic_verify_local_with_plan(
        Arc::clone(&session),
        step_context,
        tracker,
        retry,
        CancellationToken::new(),
        automatic_validation_plan(&repo_root, CHANGED_PATH, false),
    )
    .await
    .expect("later automatic fast pass");

    let receipts = ledger
        .automatic_validation_receipts_for_test(&retry_claim_id)
        .await;
    assert_eq!(receipts.len(), 1);
    assert!(receipts[0].2);
    let gate = ledger.completion_gate().await.expect("completion gate");
    assert_ne!(gate.status, TaskCompletionStatus::Passed);
    assert!(
        gate.reasons
            .iter()
            .any(|reason| reason.contains("conclusively failed"))
    );
}

fn base_args() -> serde_json::Value {
    json!({
        "mode": "fast",
        "changed": [],
        "staged": false,
        "scope_current": false,
        "no_cache": false,
        "json": false
    })
}

fn verifier_payload(verdict: &str) -> serde_json::Value {
    json!({
        "schema_version": VERIFY_LOCAL_JSON_SCHEMA_VERSION,
        "producer": VERIFY_LOCAL_JSON_PRODUCER,
        "verdict": verdict,
    })
}

fn structured_process_output(stdout: String, stderr: &str, exit_code: i32) -> ExecToolCallOutput {
    let mut output = ExecToolCallOutput {
        exit_code,
        ..Default::default()
    };
    output.stdout.text = stdout.clone();
    output.stderr.text = stderr.to_string();
    output.aggregated_output.text = format!("{stdout}{stderr}");
    output.aggregated_output_bytes = Some(output.aggregated_output.text.as_bytes().to_vec());
    output
}

#[tokio::test]
async fn command_result_keeps_tail_failure_and_binds_exact_content_addressed_output() {
    let temp = tempfile::tempdir().expect("tempdir");
    let plan = codex_verify_local::PlanEnvelopeV2::new(PlanMode::Fast, "invocation");
    let command = codex_verify_local::CommandSpecV2 {
        id: "core-tests".to_string(),
        kind: "owner_test".to_string(),
        args: vec![codex_verify_local::CommandArgV2::text("cargo test")],
        cwd: RawPath::from_utf8("."),
        timeout_ms: 1_000,
        owner_packages: vec!["codex-core".to_string()],
        hash_paths: Vec::new(),
        reason: "test".to_string(),
    };
    let mut lines = (0..9_000)
        .map(|index| format!("ordinary diagnostic line {index}"))
        .collect::<Vec<_>>();
    lines.push("error: decisive tail failure".to_string());
    let exact = lines.join("\n");
    assert!(exact.len() > 64 * 1024);
    let output = structured_process_output(exact.clone(), "", 101);

    let result = command_result_from_core_execution(
        &plan,
        &command,
        0,
        "nonce".to_string(),
        Some(&output),
        Some(101),
        temp.path(),
        "thread/verify-local",
        "cargo test -p codex-core",
    )
    .await;

    assert!(result.diagnostic.contains("error: decisive tail failure"));
    assert!(result.diagnostic.contains("Exact output: sha256:"));
    let artifact = result
        .exact_output_artifact
        .as_ref()
        .expect("exact artifact");
    assert_eq!(artifact.sha256, output_sha256(exact.as_bytes()));
    assert!(
        artifact
            .path
            .ends_with(format!("sha256-{}.log", artifact.sha256))
    );
    assert_eq!(
        tokio::fs::read(&artifact.path)
            .await
            .expect("artifact bytes"),
        exact.as_bytes()
    );
    assert_eq!(result.log_path.as_ref(), Some(&artifact.path));
    let omission = result
        .diagnostic_omission
        .expect("bounded preview omission");
    assert!(omission.bytes > 0);
    assert!(omission.lines > 0);

    let finalized = finalize_plan(plan, vec![result]);
    let contract = serialize_legacy_v1(&finalized, /*crlf*/ false).expect("contract bytes");
    let contract: serde_json::Value = serde_json::from_slice(&contract).expect("contract json");
    let receipt_artifact = &contract["results"][0]["exact_output_artifact"];
    assert_eq!(receipt_artifact["sha256"], output_sha256(exact.as_bytes()));
    let receipt_path = receipt_artifact["path"].as_str().expect("artifact handle");
    let reread = tokio::fs::read_to_string(receipt_path)
        .await
        .expect("reread exact output from contract handle");
    assert!(reread.ends_with("error: decisive tail failure"));
    assert_eq!(output_sha256(reread.as_bytes()), receipt_artifact["sha256"]);
}

#[tokio::test]
async fn command_result_artifact_preserves_non_utf8_process_bytes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let plan = codex_verify_local::PlanEnvelopeV2::new(PlanMode::Fast, "invocation");
    let command = codex_verify_local::CommandSpecV2 {
        id: "binary-output".to_string(),
        kind: "owner_test".to_string(),
        args: vec![codex_verify_local::CommandArgV2::text("binary verifier")],
        cwd: RawPath::from_utf8("."),
        timeout_ms: 1_000,
        owner_packages: vec!["codex-core".to_string()],
        hash_paths: Vec::new(),
        reason: "test".to_string(),
    };
    let exact = vec![0xff, 0xfe, b'e', b'r', b'r', b'o', b'r', b'\n'];
    let mut output = structured_process_output("��error\n".to_string(), "", 1);
    output.aggregated_output_bytes = Some(exact.clone());

    let result = command_result_from_core_execution(
        &plan,
        &command,
        0,
        "nonce".to_string(),
        Some(&output),
        Some(1),
        temp.path(),
        "thread/verify-local-binary",
        "binary verifier",
    )
    .await;

    let artifact = result.exact_output_artifact.expect("exact artifact");
    assert_eq!(artifact.sha256, output_sha256(&exact));
    assert_eq!(
        tokio::fs::read(artifact.path).await.expect("artifact"),
        exact
    );
}

#[tokio::test]
async fn incomplete_process_capture_is_not_bound_as_exact_verifier_output() {
    let temp = tempfile::tempdir().expect("tempdir");
    let plan = codex_verify_local::PlanEnvelopeV2::new(PlanMode::Fast, "invocation");
    let command = codex_verify_local::CommandSpecV2 {
        id: "incomplete-output".to_string(),
        kind: "owner_test".to_string(),
        args: vec![codex_verify_local::CommandArgV2::text("partial verifier")],
        cwd: RawPath::from_utf8("."),
        timeout_ms: 1_000,
        owner_packages: vec!["codex-core".to_string()],
        hash_paths: Vec::new(),
        reason: "test".to_string(),
    };
    let mut output = structured_process_output("partial output\n".to_string(), "", 0);
    output.output_complete = false;

    let result = command_result_from_core_execution(
        &plan,
        &command,
        0,
        "nonce".to_string(),
        Some(&output),
        Some(0),
        temp.path(),
        "thread/verify-local-incomplete",
        "partial verifier",
    )
    .await;

    assert_eq!(result.log_state, LogState::IncompleteAfterTermination);
    assert!(result.exact_output_artifact.is_none());
    assert!(result.log_path.is_some());
    assert!(
        result
            .runner_error
            .as_deref()
            .is_some_and(|error| error.contains("incomplete after termination"))
    );
    assert!(result.diagnostic.starts_with("Incomplete captured output:"));

    let finalized = finalize_plan(plan, vec![result]);
    assert_eq!(finalized.verdict, codex_verify_local::Verdict::ToolingError);
    assert!(!finalized.cache_eligible);
}

#[test]
fn broadening_field_rejection_is_model_legible() {
    let mut args = base_args();
    args["all_dirty"] = json!(true);

    let Err(FunctionCallError::RespondToModel(message)) =
        parse_verify_local_arguments(&args.to_string())
    else {
        panic!("expected model-visible rejection");
    };

    assert!(message.contains("all_dirty"));
    assert!(message.contains("broadening or mutating flags are human CLI-only"));
    assert!(message.contains("changed"));
    assert!(message.contains("scope_current"));
}

#[test]
fn unknown_field_rejection_is_model_legible() {
    let mut args = base_args();
    args["surprise"] = json!(true);

    let Err(FunctionCallError::RespondToModel(message)) =
        parse_verify_local_arguments(&args.to_string())
    else {
        panic!("expected model-visible rejection");
    };

    assert!(message.contains("surprise"));
    assert!(message.contains("only accepts read-only narrowing fields"));
}

#[test]
fn mutating_scope_field_rejection_is_model_legible() {
    let mut args = base_args();
    args["scope_add"] = json!(["codex-rs/core/src/tools/handlers/verify_local.rs"]);

    let Err(FunctionCallError::RespondToModel(message)) =
        parse_verify_local_arguments(&args.to_string())
    else {
        panic!("expected model-visible rejection");
    };

    assert!(message.contains("scope_add"));
    assert!(message.contains("broadening or mutating flags are human CLI-only"));
    assert!(message.contains("scope_current"));
}

#[test]
fn argv_is_structured_and_always_requests_versioned_json() {
    let args = parse_verify_local_arguments(
        &json!({
            "mode": "final",
            "changed": ["codex-rs/core/src/tools/spec_plan.rs", "--allow-workspace"],
            "staged": true,
            "scope_current": true,
            "no_cache": true,
            "json": false,
            "environment_id": "secondary"
        })
        .to_string(),
    )
    .expect("args parse");

    assert_eq!(
        build_verify_local_argv(&args),
        vec![
            "just",
            "verify-local",
            "--final",
            "--json",
            "--changed=codex-rs/core/src/tools/spec_plan.rs",
            "--changed=--allow-workspace",
            "--staged",
            "--scope",
            "current",
            "--no-cache",
        ]
    );

    let mut json_args = base_args();
    json_args["json"] = json!(true);
    let json_args = parse_verify_local_arguments(&json_args.to_string()).expect("args parse");
    assert_eq!(
        build_verify_local_argv(&json_args)
            .iter()
            .filter(|arg| arg.as_str() == "--json")
            .count(),
        1
    );
}

#[test]
fn verifier_modes_have_explicit_bounded_timeouts() {
    for (mode, expected) in [
        ("plan", 2 * 60 * 1_000),
        ("fast", 20 * 60 * 1_000),
        ("final", 60 * 60 * 1_000),
    ] {
        let mut raw_args = base_args();
        raw_args["mode"] = json!(mode);
        let args = parse_verify_local_arguments(&raw_args.to_string()).expect("args parse");
        assert_eq!(args.timeout_ms(), expected);
    }
}

#[test]
fn environment_id_is_parsed_but_not_forwarded_to_verifier() {
    let mut raw_args = base_args();
    raw_args["environment_id"] = json!("secondary");

    let args = parse_verify_local_arguments(&raw_args.to_string()).expect("args parse");

    assert_eq!(args.environment_id.as_deref(), Some("secondary"));
    assert!(
        !build_verify_local_argv(&args)
            .iter()
            .any(|arg| arg.contains("secondary") || arg.contains("environment"))
    );
}

#[test]
fn validation_state_directories_are_unique_and_separate() {
    let (first_guard, first_codex_home, first_sqlite_home) =
        create_isolated_validation_state().expect("first isolated state");
    let (second_guard, second_codex_home, second_sqlite_home) =
        create_isolated_validation_state().expect("second isolated state");

    assert_ne!(first_guard.path(), second_guard.path());
    assert_ne!(first_codex_home, first_sqlite_home);
    assert_ne!(first_codex_home, second_codex_home);
    assert!(first_codex_home.is_dir());
    assert!(first_sqlite_home.is_dir());
    assert!(second_codex_home.is_dir());
    assert!(second_sqlite_home.is_dir());
}

#[test]
fn handler_waits_for_shell_runtime_cancellation_cleanup() {
    let handler = VerifyLocalHandler::for_verify_local_environment_id(false);
    assert!(handler.waits_for_runtime_cancellation());
}

#[test]
fn exact_json_verdict_parsing_distinguishes_no_proof() {
    let run = parse_verify_local_run(
        verifier_payload("VERIFIED (no proof needed)").to_string(),
        String::new(),
        Some(0),
    );

    assert_eq!(
        run.verdict_text.as_deref(),
        Some("VERIFIED (no proof needed)")
    );
    assert!(run.tool_success);
}

#[test]
fn live_output_finalizer_returns_structured_verifier_result() {
    let payload = json!({
        "schema_version": 1,
        "producer": "kd4.verify_local",
        "mode": "fast",
        "verdict": "VERIFIED",
        "scope": {
            "source": "changed",
            "active_files": ["codex-rs/core/src/tools/handlers/verify_local.rs"],
            "ignored_dirty_files": [],
            "stale_reasons": []
        }
    });
    let output = FunctionToolOutput::from_text("generic shell envelope".to_string(), Some(true));
    let exec_output = structured_process_output(
        serde_json::to_string_pretty(&payload).expect("serialize payload"),
        "",
        0,
    );

    let (output, run) = finalize_verify_local_output(
        output,
        Some(&exec_output),
        Some(exec_output.exit_code),
        false,
    );

    assert_eq!(run.exit_code, Some(0));
    assert_eq!(run.verdict_text.as_deref(), Some("VERIFIED"));
    assert_eq!(output.success, Some(true));
    assert_eq!(output.post_tool_use_response, Some(payload));
    let text = output.into_text();
    assert!(text.contains("Verdict: VERIFIED"));
    assert!(text.contains("Scope: changed (1 active file(s))"));
    assert!(!text.contains("Final output:"));
}

#[test]
fn raw_json_finalizer_removes_the_generic_shell_envelope() {
    let payload = json!({
        "schema_version": 1,
        "producer": "kd4.verify_local",
        "verdict": "PLANNED",
        "scope": null
    });
    let output = FunctionToolOutput::from_text("generic shell envelope".to_string(), Some(true));
    let exec_output = structured_process_output(
        serde_json::to_string_pretty(&payload).expect("serialize payload"),
        "",
        0,
    );

    let (output, run) = finalize_verify_local_output(
        output,
        Some(&exec_output),
        Some(exec_output.exit_code),
        true,
    );

    assert!(run.tool_success);
    assert_eq!(
        serde_json::from_str::<Value>(&output.into_text()).expect("raw JSON output"),
        payload
    );
}

#[test]
fn planned_json_is_successful_but_not_proof_bearing() {
    let run = parse_verify_local_run(
        json!({
            "schema_version": VERIFY_LOCAL_JSON_SCHEMA_VERSION,
            "producer": VERIFY_LOCAL_JSON_PRODUCER,
            "mode": "plan",
            "verdict": "PLANNED",
            "planned": [
                {"id": "fmt"},
                {"id": "core-tests"},
                {"id": "schema-check"},
                {"id": "clippy"},
                {"id": "wiring"}
            ]
        })
        .to_string(),
        String::new(),
        Some(0),
    );

    assert_eq!(run.verdict_text.as_deref(), Some("PLANNED"));
    assert!(run.tool_success);
    let rendered = render_verify_local_output(&run, false);
    assert!(rendered.contains("proof-bearing validation"));
    assert!(rendered.contains("Mode: plan"));
    assert!(
        rendered.contains("Planned checks: 5 (fmt, core-tests, schema-check, clippy, +1 more)")
    );
    assert!(!rendered.contains("\"producer\""));
    assert!(!rendered.contains("\n\nStdout:"));
}

#[test]
fn verified_json_parses_scope_active_files() {
    let run = parse_verify_local_run(
        json!({
            "schema_version": VERIFY_LOCAL_JSON_SCHEMA_VERSION,
            "producer": VERIFY_LOCAL_JSON_PRODUCER,
            "verdict": "VERIFIED",
            "scope": {
                "scope_id": "changed-a",
                "source": "changed",
                "active_files": ["codex-rs/core/src/a.rs", "codex-rs/core/src/b.rs"],
                "ignored_dirty_files": ["codex-rs/core/src/c.rs"],
                "stale_reasons": []
            }
        })
        .to_string(),
        String::new(),
        Some(0),
    );

    let scope = run.scope.expect("scope parsed");
    assert_eq!(scope.source, "changed");
    assert_eq!(
        scope.active_files,
        vec![
            PathBuf::from("codex-rs/core/src/a.rs"),
            PathBuf::from("codex-rs/core/src/b.rs")
        ]
    );
    assert_eq!(
        scope.ignored_dirty_files,
        vec![PathBuf::from("codex-rs/core/src/c.rs")]
    );
}

#[test]
fn leading_tool_output_cannot_smuggle_a_verifier_json_result() {
    let run = parse_verify_local_run(
        format!("Preparing verifier...\n{}", verifier_payload("VERIFIED")),
        String::new(),
        Some(0),
    );

    assert!(run.json.is_none());
    assert_eq!(run.verdict_text, None);
    assert!(!run.tool_success);
}

#[test]
fn embedded_pretty_json_is_not_selected_from_mixed_stdout() {
    let stdout = format!(
        "Preparing verifier...\n{}",
        serde_json::to_string_pretty(&json!({
            "schema_version": VERIFY_LOCAL_JSON_SCHEMA_VERSION,
            "producer": VERIFY_LOCAL_JSON_PRODUCER,
            "verdict": "VERIFIED",
            "scope": {
                "source": "changed",
                "active_files": ["codex-rs/core/src/tools/handlers/verify_local.rs"],
                "ignored_dirty_files": [],
                "stale_reasons": []
            }
        }))
        .expect("json")
    );

    let run = parse_verify_local_run(stdout, String::new(), Some(0));

    assert!(parse_verify_local_json(&run.stdout).is_none());
    assert_eq!(run.verdict_text, None);
    assert!(!run.tool_success);
    assert!(run.scope.is_none());
}

#[test]
fn formatted_exit_code_text_cannot_override_the_process_exit_code() {
    let run = parse_verify_local_run(
        json!({
            "schema_version": VERIFY_LOCAL_JSON_SCHEMA_VERSION,
            "producer": VERIFY_LOCAL_JSON_PRODUCER,
            "verdict": "VERIFIED",
            "message": "Exit code: 0",
            "scope": {
                "source": "changed",
                "active_files": [],
                "ignored_dirty_files": [],
                "stale_reasons": []
            }
        })
        .to_string(),
        String::new(),
        Some(1),
    );

    assert_eq!(run.exit_code, Some(1));
    assert!(!run.tool_success);
}

#[test]
fn text_fallback_uses_exact_verdict_line() {
    let run = parse_verify_local_run(
        "some output\nVerdict: NEEDS_REGEN\n".to_string(),
        String::new(),
        Some(2),
    );

    assert_eq!(run.verdict_text.as_deref(), Some("NEEDS_REGEN"));
    assert!(!run.tool_success);
    assert!(render_verify_local_output(&run, false).contains("autonomous blocker"));
}

#[test]
fn nonzero_failure_is_tool_output_failure_not_parse_crash() {
    let run = parse_verify_local_run(
        verifier_payload("FAILED").to_string(),
        "assertion failed".to_string(),
        Some(1),
    );

    assert_eq!(run.verdict_text.as_deref(), Some("FAILED"));
    assert!(!run.tool_success);
    let rendered = render_verify_local_output(&run, false);
    assert!(rendered.contains("Exit code: 1"));
    assert!(rendered.contains("assertion failed"));
}

#[test]
fn only_versioned_successful_json_is_proof_bearing() {
    let scope = json!({
        "source": "changed",
        "active_files": ["codex-rs/core/src/tools/handlers/verify_local.rs"],
        "ignored_dirty_files": [],
        "stale_reasons": []
    });
    let valid = parse_verify_local_run(
        json!({
            "schema_version": VERIFY_LOCAL_JSON_SCHEMA_VERSION,
            "producer": VERIFY_LOCAL_JSON_PRODUCER,
            "verdict": "VERIFIED",
            "scope": scope,
        })
        .to_string(),
        String::new(),
        Some(0),
    );
    assert!(valid.is_proof_bearing());

    for invalid in [
        json!({
            "producer": VERIFY_LOCAL_JSON_PRODUCER,
            "verdict": "VERIFIED",
            "scope": scope,
        }),
        json!({
            "schema_version": VERIFY_LOCAL_JSON_SCHEMA_VERSION,
            "producer": "some.other.producer",
            "verdict": "VERIFIED",
            "scope": scope,
        }),
    ] {
        let run = parse_verify_local_run(invalid.to_string(), String::new(), Some(0));
        assert!(!run.tool_success);
        assert!(!run.is_proof_bearing());
        assert!(render_verify_local_output(&run, false).contains("unsupported JSON contract"));
    }

    let nonzero = parse_verify_local_run(
        json!({
            "schema_version": VERIFY_LOCAL_JSON_SCHEMA_VERSION,
            "producer": VERIFY_LOCAL_JSON_PRODUCER,
            "verdict": "VERIFIED",
            "scope": scope,
        })
        .to_string(),
        String::new(),
        Some(1),
    );
    assert!(!nonzero.tool_success);
    assert!(!nonzero.is_proof_bearing());
}

#[test]
fn malformed_scope_objects_are_never_proof_bearing() {
    let invalid_scopes = [
        json!(true),
        json!({}),
        json!({
            "source": "",
            "active_files": [],
            "ignored_dirty_files": [],
            "stale_reasons": []
        }),
        json!({
            "source": "changed",
            "active_files": [1],
            "ignored_dirty_files": [],
            "stale_reasons": []
        }),
        json!({
            "source": "changed",
            "active_files": [],
            "ignored_dirty_files": []
        }),
    ];

    for scope in invalid_scopes {
        let run = parse_verify_local_run(
            json!({
                "schema_version": VERIFY_LOCAL_JSON_SCHEMA_VERSION,
                "producer": VERIFY_LOCAL_JSON_PRODUCER,
                "verdict": "VERIFIED",
                "scope": scope,
            })
            .to_string(),
            String::new(),
            Some(0),
        );
        assert!(run.tool_success);
        assert!(run.scope.is_none());
        assert!(!run.is_proof_bearing());
    }
}
