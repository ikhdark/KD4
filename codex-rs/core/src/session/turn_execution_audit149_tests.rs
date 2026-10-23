use super::*;
use crate::plan_store::PlanExecutionSnapshot;
use crate::plan_store::PlanLineage;
use crate::plan_store::acceptance::AcceptedEvidence;
use serde_json::json;

fn require_inspection(control: &mut TurnExecutionControl) {
    let mut lineage = PlanLineage::default();
    lineage.acceptance.insert("inspect".into(), serde_json::from_value(json!({
        "text":"Inspect final source", "origin":{"turn_id":"task","sha256":"origin"},
        "requirement":{"kind":"inspection","environment_id":"local","path":"source.rs","start":0,"end":10}
    })).unwrap());
    let plan = UpdatePlanArgs {
        explanation: None,
        plan: Vec::new(),
    };
    control.refresh_plan(Some(PlanExecutionSnapshot::new(&plan, &lineage)));
}

fn source(hash: &str, start: u64, end: u64) -> Value {
    json!({"source":"read_file",
        "scope":{"environment":"local","path":"source.rs"},
        "identity":{"sha256":hash,"ranges":[[start,end]],"values":[]}})
}

fn recovery(artifact: &str, hash: &str, start: u64, end: u64) -> Value {
    json!({"source":"artifact","scope":artifact,
        "identity":{"sha256":hash,"ranges":[[start,end]],"values":[]}})
}

fn record_source(
    collector: &SamplingRequestSignalCollector,
    evidence: Value,
    artifact: Option<&str>,
    admitted_revision: u64,
) {
    let name = if evidence["source"] == "artifact" {
        "read_tool_output"
    } else {
        "read_file"
    };
    collector.record_code_mode_result(CodeModeToolResult {
        cell_id: "inspection",
        tool_name: &ToolName::plain(name),
        payload: &ToolPayload::Function {
            arguments: json!({"path":"source.rs"}).to_string(),
        },
        source_dependencies: None,
        outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
        signal: Some(&json!({"semantic_evidence":evidence,
            "source_artifact_id":artifact,"validation_mutation_revision":admitted_revision})),
        result: &json!({"complete":true}),
        canonical_artifact_required: false,
    });
}

fn settled(revision: u64) -> SamplingRequestSettledState {
    SamplingRequestSettledState {
        mutation_revision: revision,
        attributed_mutation_revision: revision,
        tool_exposure_revision: 0,
    }
}

fn ranges(control: &TurnExecutionControl, revision: u64) -> Vec<Vec<(u64, u64)>> {
    control
        .acceptance_evidence("task", revision)
        .observations
        .values()
        .filter_map(|evidence| match evidence {
            AcceptedEvidence::Inspection { ranges, .. } => Some(ranges.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn audit149_late_contract_reuses_already_delivered_source() {
    let mut control = TurnExecutionControl::new();
    let baseline = control.baselines(0);
    let collector = control.collector(&baseline);
    record_source(&collector, source("hash-a", 0, 10), Some("snapshot"), 0);
    control.observe_progress(&baseline, &collector, &settled(0));
    require_inspection(&mut control);
    assert_eq!(ranges(&control, 0), [vec![(0, 10)]]);
    assert_eq!(
        collector.state.lock().unwrap().code_mode_nested_tool_count,
        1,
        "declaring acceptance must not require another native read"
    );
    assert!(
        ranges(&control, 1).is_empty(),
        "later edits invalidate the read"
    );
}

#[test]
fn audit149_recovered_inspection_uses_one_native_read_and_does_not_refresh_it() {
    let mut control = TurnExecutionControl::new();
    require_inspection(&mut control);
    let baseline = control.baselines(0);
    let collector = control.collector(&baseline);
    record_source(&collector, source("hash-a", 0, 4), Some("snapshot"), 0);
    record_source(&collector, recovery("snapshot", "hash-a", 4, 10), None, 0);
    control.observe_progress(&baseline, &collector, &settled(0));
    assert_eq!(ranges(&control, 0), [vec![(0, 10)]]);
    let state = collector.state.lock().unwrap();
    assert_eq!(
        state
            .outcomes
            .iter()
            .filter(|outcome| outcome
                .source_evidence
                .as_ref()
                .is_some_and(|evidence| evidence["source"] == "read_file"))
            .count(),
        1
    );
    drop(state);
    assert!(ranges(&control, 1).is_empty());
    let baseline = control.baselines(1);
    let collector = control.collector(&baseline);
    record_source(&collector, recovery("snapshot", "hash-a", 0, 10), None, 1);
    control.observe_progress(&baseline, &collector, &settled(1));
    assert!(
        ranges(&control, 1).is_empty(),
        "historical recovery cannot refresh source"
    );
}

#[test]
fn audit149_new_source_version_retracts_old_acceptance_and_recovery_cannot_resurrect_it() {
    let mut control = TurnExecutionControl::new();
    require_inspection(&mut control);
    let baseline = control.baselines(0);
    let collector = control.collector(&baseline);
    record_source(&collector, source("hash-a", 0, 10), Some("old-snapshot"), 0);
    record_source(&collector, source("hash-b", 0, 4), Some("new-snapshot"), 0);
    record_source(
        &collector,
        recovery("old-snapshot", "hash-a", 0, 10),
        None,
        0,
    );
    record_source(
        &collector,
        recovery("new-snapshot", "hash-a", 4, 10),
        None,
        0,
    );
    control.observe_progress(&baseline, &collector, &settled(0));
    assert_eq!(
        ranges(&control, 0),
        [vec![(0, 4)]],
        "old or mismatched snapshots must not satisfy the new source version"
    );
}

#[test]
fn audit149_post_edit_reads_use_per_call_admission_not_request_baseline() {
    for (admitted, expected) in [(0, Vec::new()), (1, vec![vec![(0, 10)]])] {
        let mut control = TurnExecutionControl::new();
        require_inspection(&mut control);
        let baseline = control.baselines(0);
        let collector = control.collector(&baseline);
        record_source(&collector, source("final", 0, 10), None, admitted);
        control.observe_progress(&baseline, &collector, &settled(1));
        assert_eq!(
            ranges(&control, 1),
            expected,
            "read-before-edit is invalid; patch-then-read observes final source"
        );
    }
}

#[test]
fn audit149_blocked_provider_does_not_hide_independent_validation() {
    let readiness = CompletionReadiness {
        validation_work: vec!["Repair local consumer test".into()],
        ..Default::default()
    };
    assert_eq!(
        readiness.remaining(true, &["provider".into()]),
        ["Repair local consumer test"]
    );
}

#[test]
fn audit149_validation_blockers_are_scoped_to_exact_checks() {
    let readiness = CompletionReadiness {
        live_work: vec!["Consume owned validation".into()],
        validation_work: vec!["Provider check".into(), "Local consumer".into()],
        validation_check_ids: vec!["provider-check".into(), "consumer-check".into()],
        validation_acceptance_ids: BTreeMap::from([(
            "provider-check".into(),
            vec!["provider".into()],
        )]),
        ..Default::default()
    };
    assert_eq!(
        readiness.remaining(true, &["provider".into()]),
        ["Consume owned validation", "Local consumer"]
    );
    assert_eq!(readiness.remaining(false, &["provider".into()]).len(), 3);
    assert_eq!(readiness.remaining(true, &["unrelated".into()]).len(), 3);
}
#[test]
fn audit149_required_test_evidence_comes_from_one_runner_execution_not_scope_claims() {
    for authenticated in [false, true] {
        let mut control = TurnExecutionControl::new();
        require_inspection(&mut control);
        let baseline = control.baselines(0);
        let collector = control.collector(&baseline);
        let identity = crate::validation::RunnerTestIdentity {
            binary: "codex-core".into(),
            helpers: Vec::new(),
            test: "consumer_regression".into(),
        };
        let mut signal = json!({
            "command_validation":{"validation":true,"proof":true,"tests":true,
                "execution_context":{"environment_id":"local","cwd":"repo","environment_fingerprint":"env"}},
            "validation_mutation_revision":0,
            "validation_scope":{"environment_id":"local","paths":[{"path":"consumer.rs","recursive":false}],
                "test_execution":true,"passed_tests":[identity]},
            "runner_execution_receipt":{"executed_tests":1,"exit_code":0}
        });
        if authenticated {
            signal["runner_execution_receipt"]["runner_input_fingerprint"] = json!("inputs");
            signal["runner_execution_receipt"]["dependency_manifest"] =
                json!({"execution_context_sha256":"execution"});
            signal["runner_execution_receipt"]["execution_configuration"] =
                json!({"profile":"fast"});
            signal["runner_execution_receipt"]["executions"] = json!([identity]);
        }
        let payload = ToolPayload::Function {
            arguments: json!({"cmd":"python -m unittest tests.consumer"}).to_string(),
        };
        let ordinal = collector
            .register_deterministic_tool_call(
                &ToolName::plain("exec_command"),
                &payload,
                "consumer",
            )
            .ordinal;
        collector.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            Some(signal),
            &codex_protocol::models::ResponseInputItem::FunctionCallOutput {
                call_id: "consumer".into(),
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    "Ran 1 test".into(),
                ),
            },
            false,
        );
        control.settle(&baseline, &collector, &settled(0));
        let evidence = control.acceptance_evidence("task", 0);
        let observed = evidence
            .observations
            .values()
            .find_map(|item| match item {
                AcceptedEvidence::Validation { passed_tests, .. } => Some(passed_tests),
                _ => None,
            })
            .expect("successful validation has attributed scope");
        assert_eq!(
            observed.contains(&identity),
            authenticated,
            "scope assertions or aggregate counts must never invent exact passing test identities"
        );
    }
}
#[test]
fn audit149_late_older_read_does_not_retract_newer_acceptance_or_hide_novel_bytes() {
    let mut control = TurnExecutionControl::new();
    require_inspection(&mut control);
    let baseline = control.baselines(0);
    let collector = control.collector(&baseline);
    record_source(&collector, source("new", 0, 10), Some("new-snapshot"), 1);
    record_source(&collector, source("old", 0, 4), Some("old-snapshot"), 0);
    let progress = control.observe_progress(&baseline, &collector, &settled(1));
    assert_eq!(ranges(&control, 1), [vec![(0, 10)]]);
    assert!(progress.contains(&TurnTimingProgressKind::NewSourceEvidence));
    assert_eq!(
        control.delivered_coverage.len(),
        4,
        "both native snapshots and their artifact aliases retain historical bytes"
    );
}
