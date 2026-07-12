use super::*;
use crate::function_tool::FunctionCallError;
use serde_json::json;

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
    let output = FunctionToolOutput::from_text(
        format!(
            "Wall time: 1.25 seconds\nProcess exited with code 0\nFinal output:\n{}",
            serde_json::to_string_pretty(&payload).expect("serialize payload")
        ),
        Some(true),
    );

    let (output, run) = finalize_verify_local_output(output, false);

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
    let output = FunctionToolOutput::from_text(
        format!(
            "Wall time: 0.5 seconds\nProcess exited with code 0\nFinal output:\n{}",
            serde_json::to_string_pretty(&payload).expect("serialize payload")
        ),
        Some(true),
    );

    let (output, run) = finalize_verify_local_output(output, true);

    assert!(run.tool_success);
    assert_eq!(
        serde_json::from_str::<Value>(&output.into_text()).expect("raw JSON output"),
        payload
    );
}

#[test]
fn planned_json_is_successful_but_not_proof_bearing() {
    let run = parse_verify_local_run(
        verifier_payload("PLANNED").to_string(),
        String::new(),
        Some(0),
    );

    assert_eq!(run.verdict_text.as_deref(), Some("PLANNED"));
    assert!(run.tool_success);
    assert!(render_verify_local_output(&run, false).contains("proof-bearing validation"));
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
fn json_verdict_parses_after_leading_tool_output() {
    let run = parse_verify_local_run(
        format!("Preparing verifier...\n{}", verifier_payload("VERIFIED")),
        String::new(),
        Some(0),
    );

    assert_eq!(run.verdict_text.as_deref(), Some("VERIFIED"));
    assert!(run.tool_success);
}

#[test]
fn pretty_json_scope_parses_after_leading_tool_output() {
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

    let parsed = parse_verify_local_json(&run.stdout).expect("json parsed");
    assert_eq!(
        parsed.get("verdict").and_then(Value::as_str),
        Some("VERIFIED")
    );
    assert_eq!(run.verdict_text.as_deref(), Some("VERIFIED"));
    assert!(run.tool_success);
    assert_eq!(
        run.scope.expect("scope parsed").active_files,
        vec![PathBuf::from(
            "codex-rs/core/src/tools/handlers/verify_local.rs"
        )]
    );
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
            "scope": scope.clone(),
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
            "scope": scope.clone(),
        }),
        json!({
            "schema_version": VERIFY_LOCAL_JSON_SCHEMA_VERSION,
            "producer": "some.other.producer",
            "verdict": "VERIFIED",
            "scope": scope.clone(),
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
