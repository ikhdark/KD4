use super::*;
use serde_json::json;

#[test]
fn audit148_summary_is_not_test_execution_but_producer_observation_is() {
    for (signal, executed) in [
        (json!({"validation_summary_tests": 98}), false),
        (
            json!({"test_execution_observation": {
                "kind": "producer_test_execution_observation_v1",
                "executed_tests": 1, "exit_code": 0
            }}),
            true,
        ),
        (
            json!({"test_execution_observation": {
                "kind": "producer_test_execution_observation_v1",
                "executed_tests": 0, "exit_code": 0
            }}),
            false,
        ),
    ] {
        let outcome = SamplingToolOutcome::from_signal(
            0,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            Some(&signal),
        );
        assert_eq!(outcome.tests_executed, executed);
    }
}

#[test]
fn audit148_handler_errors_do_not_create_orphaned_validation_failures() {
    let mut control = TurnExecutionControl::new();
    let baseline = control.baselines(0);
    let collector = control.collector(&baseline);
    let tool = ToolName::plain("exec_command");
    let payload = ToolPayload::Function {
        arguments: json!({"cmd": "cargo check -p example"}).to_string(),
    };
    let ordinal = collector
        .register_deterministic_tool_call(&tool, &payload, "preflight")
        .ordinal;
    collector.record_failure(ordinal, "process creation failed before execution", false);
    let settled = SamplingRequestSettledState {
        mutation_revision: 0,
        attributed_mutation_revision: 0,
        tool_exposure_revision: 0,
    };
    control.settle(&baseline, &collector, &settled);
    assert!(control.failed_validation_checks.is_empty());
    assert!(control.completion_readiness(0).validation_work.is_empty());

    // An actual failed command remains a blocker, even without test bodies.
    let baseline = control.baselines(0);
    let collector = control.collector(&baseline);
    let ordinal = collector
        .register_deterministic_tool_call(&tool, &payload, "compiler")
        .ordinal;
    collector.record_response_result(
        ordinal,
        ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
        Some(json!({"command_validation": {"validation": true, "proof": true, "tests": false}})),
        &ResponseInputItem::FunctionCallOutput {
            call_id: "compiler".into(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                "compiler error".into(),
            ),
        },
        false,
    );
    control.settle(&baseline, &collector, &settled);
    assert_eq!(control.failed_validation_checks.len(), 1);
}

#[test]
fn audit148_refused_runner_validation_stays_pending_until_a_proving_rerun() {
    let settled = SamplingRequestSettledState {
        mutation_revision: 0,
        attributed_mutation_revision: 0,
        tool_exposure_revision: 0,
    };
    let tool = ToolName::plain("exec_command");
    // Call arguments cannot classify a repository recipe; only its launch owner can.
    let payload = ToolPayload::Function {
        arguments: json!({"cmd": "just check-lane codex-core --tests"}).to_string(),
    };
    for validation_proof in [true, false] {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let collector = control.collector(&baseline);
        let ordinal = collector
            .register_deterministic_tool_call(&tool, &payload, "refused")
            .ordinal;
        collector.record_admission_pending(ordinal, validation_proof);
        control.settle(&baseline, &collector, &settled);
        assert!(control.failed_validation_checks.is_empty());
        assert_eq!(
            control.admission_pending_checks.len(),
            usize::from(validation_proof)
        );
        assert_eq!(
            control
                .completion_assessment(0)
                .verification_gaps
                .iter()
                .any(|gap| gap.contains("refused admission")),
            validation_proof
        );
        if !validation_proof {
            continue;
        }

        // The eventual proving run of the same launch retires the obligation.
        let baseline = control.baselines(0);
        let collector = control.collector(&baseline);
        let ordinal = collector
            .register_deterministic_tool_call(&tool, &payload, "rerun")
            .ordinal;
        collector.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            Some(json!({"command_validation": {"validation": true, "proof": true, "tests": false}})),
            &ResponseInputItem::FunctionCallOutput {
                call_id: "rerun".into(),
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    "checked".into(),
                ),
            },
            false,
        );
        control.settle(&baseline, &collector, &settled);
        assert!(control.admission_pending_checks.is_empty());
    }

    // A refusal relayed through a response signal carries the same capability.
    let outcome = SamplingToolOutcome::from_signal(
        0,
        ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
        None,
        Some(&json!({
            "outcome": "blocked",
            "validation_admission_pending": true,
            "validation_admission_proof": true
        })),
    );
    assert!(outcome.admission_pending && outcome.admission_proof);
}
