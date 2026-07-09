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
fn argv_is_structured_and_always_includes_internal_json() {
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
fn exact_json_verdict_parsing_distinguishes_no_proof() {
    let run = parse_verify_local_run(
        json!({ "verdict": "VERIFIED (no proof needed)" }).to_string(),
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
fn planned_json_is_successful_but_not_proof_bearing() {
    let run = parse_verify_local_run(
        json!({ "verdict": "PLANNED" }).to_string(),
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
        format!(
            "Preparing verifier...\n{}",
            json!({ "verdict": "VERIFIED" })
        ),
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
        json!({ "verdict": "FAILED" }).to_string(),
        "assertion failed".to_string(),
        Some(1),
    );

    assert_eq!(run.verdict_text.as_deref(), Some("FAILED"));
    assert!(!run.tool_success);
    let rendered = render_verify_local_output(&run, false);
    assert!(rendered.contains("Exit code: 1"));
    assert!(rendered.contains("assertion failed"));
}
