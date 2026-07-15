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
