use super::*;
use codex_protocol::models::FunctionCallOutputPayload;
use serde_json::json;

#[test]
fn investigation_distinct_source_reads_trigger_checkpoint_and_nonprogress_telemetry() {
    let timing = Arc::new(TurnTimingState::default());
    timing.mark_turn_started();
    let mut control = TurnExecutionControl::new_with_timing(Arc::clone(&timing));
    let investigation = Arc::new(Mutex::new(
        crate::plan_store::investigation::InvestigationState::default(),
    ));
    let report = crate::plan_store::investigation::tests::report();
    investigation.lock().unwrap().commit(report, false);
    let baseline = control.baselines(0);
    for index in 0..3 {
        let mut pending = None;
        timing.begin_model_generation_with_metadata(
            &mut pending,
            &codex_protocol::protocol::SessionSource::Cli,
            Some(TurnTimingGenerationPurpose::FailureDiagnosis),
            TurnTimingGenerationDisposition::DecisionBearing,
            None,
        );
        drop(timing.begin_model_request_wait());
        let collector = control.collector(&baseline);
        collector.attach_investigation(Arc::clone(&investigation));
        let call_id = format!("read-{index}");
        let payload = ToolPayload::Function {
            arguments: json!({"path":format!("different-{index}.rs")}).to_string(),
        };
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("read_file"),
            &payload,
            &call_id,
        );
        let text = format!("brand new fact {index}");
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            Some(crate::tools::context::semantic_evidence_sampling_signal(
                json!(crate::tools::context::semantic_evidence_for_command_output(
                    text.as_bytes()
                )),
            )),
            &ResponseInputItem::FunctionCallOutput {
                call_id,
                output: FunctionCallOutputPayload::from_text(text),
            },
            false,
        );
        let progress = control.observe_progress(
            &baseline,
            &collector,
            &SamplingRequestSettledState {
                mutation_revision: 0,
                tool_exposure_revision: 0,
            },
        );
        assert!(progress.is_empty(), "new source is not causal narrowing");
        timing.record_generation_outcome(progress, collector.structured_action_fingerprint(), true);
        let directive = control.take_soft_convergence_directive(true);
        assert_eq!(directive.is_some(), index == 2);
        if let Some(directive) = directive {
            assert!(directive.starts_with("Investigation saturation checkpoint:"));
        }
    }
    let timing = timing.complete_snapshot().protocol_timing();
    assert_eq!(
        timing.observational_nonprogress_latency.logical_generations,
        3
    );
    assert_eq!(
        timing.observational_nonprogress_tokens.logical_generations,
        3
    );
}
