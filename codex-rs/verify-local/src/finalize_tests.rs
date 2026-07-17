use super::*;
use crate::model::CommandArgV2;
use crate::model::CommandSpecV2;
use crate::model::PlanEnvelopeV2;
use crate::model::RawPath;

fn plan() -> PlanEnvelopeV2 {
    let mut plan = PlanEnvelopeV2::new(PlanMode::Fast, "invocation");
    plan.commands.push(CommandSpecV2 {
        id: "owner:test".to_string(),
        kind: "owner_test".to_string(),
        args: vec![CommandArgV2::text("true")],
        cwd: RawPath::from_utf8("."),
        timeout_ms: 1_000,
        owner_packages: Vec::new(),
        hash_paths: Vec::new(),
        reason: String::new(),
    });
    plan
}

fn result() -> CommandResultV2 {
    CommandResultV2 {
        schema_version: 2,
        invocation_id: "invocation".to_string(),
        command_id: "owner:test".to_string(),
        command_ordinal: 0,
        runner_nonce: "nonce".to_string(),
        exit_code: Some(0),
        signal: None,
        duration_ns: 1,
        timed_out: false,
        cancelled: false,
        runner_error: None,
        launch_error: None,
        log_state: LogState::Complete,
        log_path: None,
        diagnostic: String::new(),
        exact_output_artifact: None,
        diagnostic_omission: None,
        cached: false,
        flaky: false,
        baseline: None,
    }
}

#[test]
fn successful_result_is_verified_and_cache_eligible() {
    let finalized = finalize_plan(plan(), vec![result()]);
    assert_eq!(finalized.verdict, Verdict::Verified);
    assert_eq!(finalized.exit_code, 0);
    assert!(finalized.cache_eligible);
}

#[test]
fn incomplete_success_is_a_tooling_error_and_not_cacheable() {
    let mut result = result();
    result.log_state = LogState::IncompleteAfterTermination;
    let finalized = finalize_plan(plan(), vec![result]);
    assert_eq!(finalized.verdict, Verdict::ToolingError);
    assert!(!finalized.cache_eligible);
}

#[test]
fn timeout_precedes_incomplete_after_termination() {
    let mut result = result();
    result.timed_out = true;
    result.exit_code = None;
    result.log_state = LogState::IncompleteAfterTermination;
    let finalized = finalize_plan(plan(), vec![result]);
    assert_eq!(finalized.verdict, Verdict::Inconclusive);
}

#[test]
fn identity_mismatch_is_a_tooling_error() {
    let mut result = result();
    result.command_id = "different".to_string();
    let finalized = finalize_plan(plan(), vec![result]);
    assert_eq!(finalized.verdict, Verdict::ToolingError);
    assert!(finalized.finalization_error.is_some());
}

#[test]
fn nonzero_exit_is_failed() {
    let mut result = result();
    result.exit_code = Some(9);
    assert_eq!(finalize_plan(plan(), vec![result]).verdict, Verdict::Failed);
}

#[test]
fn cancellation_is_inconclusive() {
    let mut result = result();
    result.exit_code = None;
    result.cancelled = true;
    assert_eq!(
        finalize_plan(plan(), vec![result]).verdict,
        Verdict::Inconclusive
    );
}

#[test]
fn runner_and_log_failures_are_tooling_errors() {
    let mut runner = result();
    runner.runner_error = Some("transport".to_string());
    assert_eq!(
        finalize_plan(plan(), vec![runner]).verdict,
        Verdict::ToolingError
    );
    for state in [
        LogState::IoFailure,
        LogState::FramingFailure,
        LogState::IntegrityFailure,
    ] {
        let mut failed = result();
        failed.log_state = state;
        assert_eq!(
            finalize_plan(plan(), vec![failed]).verdict,
            Verdict::ToolingError
        );
    }
}

#[test]
fn command_not_found_is_failed_but_other_launch_errors_are_tooling_errors() {
    let mut missing = result();
    missing.exit_code = None;
    missing.runner_error = None;
    missing.launch_error = Some(LaunchErrorKind::CommandNotFound);
    assert_eq!(
        finalize_plan(plan(), vec![missing]).verdict,
        Verdict::Failed
    );
    let mut denied = result();
    denied.exit_code = None;
    denied.runner_error = None;
    denied.launch_error = Some(LaunchErrorKind::PermissionDenied);
    assert_eq!(
        finalize_plan(plan(), vec![denied]).verdict,
        Verdict::ToolingError
    );
}

#[test]
fn missing_extra_duplicate_and_reordered_results_fail_closed() {
    assert_eq!(
        finalize_plan(plan(), Vec::new()).verdict,
        Verdict::ToolingError
    );
    let mut two = plan();
    let mut second_command = two.commands[0].clone();
    second_command.id = "owner:second".to_string();
    two.commands.push(second_command);
    assert_eq!(
        finalize_plan(two.clone(), vec![result()]).verdict,
        Verdict::ToolingError
    );
    assert_eq!(
        finalize_plan(two.clone(), vec![result(), result()]).verdict,
        Verdict::ToolingError
    );
    let mut first = result();
    let mut second = result();
    first.command_id = "owner:second".to_string();
    first.command_ordinal = 1;
    second.command_ordinal = 0;
    assert_eq!(
        finalize_plan(two, vec![first, second]).verdict,
        Verdict::ToolingError
    );
    assert_eq!(
        finalize_plan(plan(), vec![result(), result()]).verdict,
        Verdict::ToolingError
    );
}

#[test]
fn preexecution_verdict_and_exit_code_are_rust_owned() {
    for (verdict, exit_code) in [
        (Verdict::VerifiedNoProof, 0),
        (Verdict::NeedsScope, 3),
        (Verdict::ToolingError, 4),
        (Verdict::NeedsRegen, 5),
    ] {
        let mut preplanned = plan();
        preplanned.verdict = Some(verdict);
        let finalized = finalize_plan(preplanned, Vec::new());
        assert_eq!(finalized.verdict, verdict);
        assert_eq!(finalized.exit_code, exit_code);
    }
}

#[test]
fn nonexecuting_plans_reject_supplied_results() {
    let mut plan_mode = plan();
    plan_mode.mode = PlanMode::Plan;

    let mut preexecution_verdict = plan();
    preexecution_verdict.verdict = Some(Verdict::NeedsScope);

    let mut no_commands = plan();
    no_commands.commands.clear();

    for plan in [plan_mode, preexecution_verdict, no_commands] {
        let finalized = finalize_plan(plan, vec![result()]);
        assert_eq!(finalized.verdict, Verdict::ToolingError);
        assert_eq!(finalized.exit_code, 4);
        assert!(!finalized.cache_eligible);
        assert_eq!(finalized.results.len(), 1);
        assert_eq!(
            finalized.finalization_error.as_deref(),
            Some("execution results were supplied for a non-executing plan")
        );
    }
}
