use super::*;
use serde_json::json;

fn settle_result(control: &mut TurnExecutionControl, signal: Value, failed: bool, nested: bool) {
    settle_result_at_revision(control, signal, failed, nested, 0);
}

fn settle_result_at_revision(control: &mut TurnExecutionControl, signal: Value, failed: bool, nested: bool, revision: u64) {
    let baseline = control.baselines(revision);
    let collector = control.collector(&baseline);
    let tool = ToolName::plain("exec_command");
    let payload = ToolPayload::Function {
        arguments: json!({"cmd":"cargo check -p example"}).to_string(),
    };
    let result = json!({"exit_code":if failed { 1 } else { 0 },
        "output":if failed { "error[E0308]: mismatched types in consumer.rs" } else { "checked" }});
    let outcome = ToolOutputOutcomeContext::new(if failed {
        ToolOutputOutcome::Failure
    } else { ToolOutputOutcome::Success });
    if nested {
        collector.record_code_mode_result(CodeModeToolResult {
            cell_id: "policy-cell", tool_name: &tool, payload: &payload,
            source_dependencies: None, outcome_context: outcome,
            signal: Some(&signal), result: &result, canonical_artifact_required: false,
        });
    } else {
        let ordinal = collector.register_deterministic_tool_call(&tool, &payload, "policy-check").ordinal;
        collector.record_response_result(ordinal, outcome, Some(signal),
            &ResponseInputItem::FunctionCallOutput {
                call_id: "policy-check".into(),
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(result.to_string()),
            }, false);
    }
    control.settle(&baseline, &collector, &SamplingRequestSettledState {
        mutation_revision: revision, attributed_mutation_revision: 0, tool_exposure_revision: 0,
    });
}

#[test]
fn validation_policy_controls_repairs_without_erasing_real_failures() {
    for (mode, exhausted, continuations) in [
        (ValidationMode::Off, false, 0),
        (ValidationMode::Compile, false, 0),
        (ValidationMode::Compile, true, 0),
        (ValidationMode::Full, false, 0),
        (ValidationMode::Full, true, 0),
    ] {
        let mut control = TurnExecutionControl::new();
        settle_result(&mut control, json!({"command_validation":{
            "validation":true,"proof":true,"compile":true,"tests":false,
        }}), true, false);
        control.set_validation_policy(mode, exhausted);
        let readiness = control.completion_readiness(0);
        assert_eq!(usize::from(!readiness.remaining(false, &[]).is_empty()), continuations);
        assert_eq!(control.failed_validation_checks.len(), 1);
        assert!(control.completion_assessment(0).failed_checks.iter()
            .any(|failure| failure.contains("error[E0308]")));
    }
}

#[test]
fn policy_refusal_direct_and_nested_never_becomes_busy_or_failed_validation() {
    for nested in [false, true] {
        let mut control = TurnExecutionControl::new();
        settle_result(&mut control, json!({"validation_policy_refusal":true,
            "validation_mode":"off","reason":"validation disabled"}), true, nested);
        assert!(control.failed_validation_checks.is_empty());
        assert!(control.pending_validation_processes.is_empty());
        assert!(control.completion_readiness(0).remaining(false, &[]).is_empty());
    }
}

#[test]
fn changed_paths_do_not_create_validation_requirements() {
    let mut control = TurnExecutionControl::new();
    control.set_validation_policy(ValidationMode::Compile, false);
    let report = control.completion_assessment_with_changed_paths(
        1, Some(&[("local".into(), "consumer.rs".into())]), false,
    );
    assert!(report.verification_gaps.is_empty());
    assert!(control.completion_readiness(1).remaining(false, &[]).is_empty());
}

#[test]
fn disabled_or_exhausted_validation_still_consumes_owned_live_results() {
    for mode in [ValidationMode::Off, ValidationMode::Compile, ValidationMode::Full] {
        let mut control = TurnExecutionControl::new();
        settle_result(&mut control, json!({"command_validation":{
            "validation":true,"proof":true,"compile":true,"tests":false,
        },"background_process_id":7}), false, false);
        control.set_validation_policy(mode, true);
        let readiness = control.completion_readiness(0);
        assert_eq!(readiness.live_work.len(), 1);
        assert!(readiness.validation_work.is_empty());
    }
}

#[test]
fn controlled_compile_replay_counts_admission_refusal_and_terminal_boundaries() {
    use crate::tools::handlers::command_preflight::FullSuiteBudget;
    let started = std::time::Instant::now();
    let budget = Arc::new(FullSuiteBudget::default());
    let mut control = TurnExecutionControl::new();
    let mut refused = 0;
    // The same failure is attempted repeatedly; only a changed input permits
    // its second execution. Further changed inputs cannot reopen the budget.
    for (index, revision) in ["source-0", "source-0", "source-1", "source-2"].into_iter().enumerate() {
        let owner = format!("replay-{index}");
        match budget.reserve_policy(owner.clone(), "cargo check -p example".into(), revision.into(), 2, true) {
            Ok(permit) => {
                budget.start(&owner, None).unwrap();
                settle_result(&mut control, json!({"command_validation":{
                    "validation":true,"proof":true,"compile":true,"tests":false,
                }}), true, false);
                drop(permit);
            }
            Err(_) => {
                refused += 1;
                settle_result(&mut control, json!({"validation_policy_refusal":true,
                    "validation_mode":"compile", "reason":"bounded replay refusal"}), true, false);
            }
        }
        control.set_validation_policy(ValidationMode::Compile, budget.policy_snapshot(2).exhausted);

    }
    let admitted = budget.policy_snapshot(2).admitted;
    let terminal_reports = usize::from(control.completion_readiness(0).remaining(false, &[]).is_empty()
        && !control.completion_assessment(0).failed_checks.is_empty());
    assert_eq!((admitted, refused, terminal_reports), (2, 2, 1));
    assert_eq!(control.failed_validation_checks.len(), 1);
    println!("controlled_compile_replay admitted={admitted} refused={refused} terminal_reports={terminal_reports} elapsed_us={}", started.elapsed().as_micros());
}


#[tokio::test]
async fn attributed_rust_inputs_do_not_request_automatic_compilation() {
    for (path, rust_input) in [("README.md", false), ("src/lib.rs", true), ("Cargo.toml", true)] {
        let directory = tempfile::tempdir().unwrap();
        let cwd = codex_utils_path_uri::PathUri::from_host_native_path(directory.path()).unwrap();
        let mut tracker = crate::turn_diff_tracker::TurnDiffTracker::new();
        let delta = codex_apply_patch::apply_patch(
            &format!("*** Begin Patch\n*** Add File: {path}\n+changed\n*** End Patch"),
            &cwd, &mut Vec::new(), &mut Vec::new(),
            codex_exec_server::LOCAL_FS.as_ref(), None,
        ).await.unwrap();
        tracker.track_delta("local", &delta);
        let revision = tracker.current_mutation_revision();
        let mut control = TurnExecutionControl::new();
        control.set_validation_policy(ValidationMode::Compile, false);
        let readiness = control.completion_readiness(revision);
        assert!(readiness.validation_work.is_empty(), "source edits do not authorize compilation");
        if rust_input {
            settle_result_at_revision(&mut control, json!({"command_validation":{
                "validation":true,"proof":true,"compile":true,"tests":false,
            }}), false, false, revision);
            assert_eq!(control.validated_mutation_revision, Some(revision));
        }
        control.set_validation_policy(ValidationMode::Off, false);
        assert!(control.completion_readiness(revision).validation_work.is_empty());
    }
}
