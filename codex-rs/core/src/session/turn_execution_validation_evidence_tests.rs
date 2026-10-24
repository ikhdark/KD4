use super::*;
use serde_json::json;


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

    // An actual failed command remains reportable, but never blocks completion.
    let baseline = control.baselines(0);
    let collector = control.collector(&baseline);
    let ordinal = collector
        .register_deterministic_tool_call(&tool, &payload, "compiler")
        .ordinal;
    collector.record_response_result(
        ordinal,
        ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
        Some(json!({"command_validation": {"validation": true, "tests": false}})),
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
    assert!(control.completion_readiness(0).remaining(false, &[]).is_empty());
}

