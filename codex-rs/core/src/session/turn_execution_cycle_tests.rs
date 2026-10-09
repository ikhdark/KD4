use super::*;
use codex_protocol::models::FunctionCallOutputPayload;
use serde_json::json;

fn settled(revision: u64) -> SamplingRequestSettledState {
    SamplingRequestSettledState {
        mutation_revision: revision,
        attributed_mutation_revision: revision,
        tool_exposure_revision: 0,
    }
}

fn payload(arguments: &str) -> ToolPayload {
    ToolPayload::Function {
        arguments: arguments.to_string(),
    }
}

fn response(text: &str) -> ResponseInputItem {
    ResponseInputItem::FunctionCallOutput {
        call_id: "call".to_string(),
        output: FunctionCallOutputPayload::from_text(text.to_string()),
    }
}

fn record(
    collector: &SamplingRequestSignalCollector,
    name: &str,
    arguments: &str,
    outcome: ToolOutputOutcome,
    signal: Option<Value>,
) {
    let call = collector.register_deterministic_tool_call(
        &ToolName::plain(name),
        &payload(arguments),
        "call",
    );
    collector.record_response_result(
        call.ordinal,
        ToolOutputOutcomeContext::new(outcome),
        signal,
        &response("unchanged"),
        false,
    );
}

fn source(collector: &SamplingRequestSignalCollector, name: &str) {
    record(
        collector,
        "read_file",
        &json!({"path": name}).to_string(),
        ToolOutputOutcome::Success,
        Some(json!({"semantic_evidence": {
            "source": "read_file", "scope": name, "identity": "same-bytes"
        }})),
    );
}

#[test]
fn uncertain_workspace_changes_preserve_cache_freshness_but_do_not_credit_progress() {
    let mut control = TurnExecutionControl::new();
    let mut tracker = crate::turn_diff_tracker::TurnDiffTracker::new();
    for index in 0..3 {
        let mut baseline = control.baselines(tracker.current_mutation_revision());
        baseline.set_attributed_mutation_revision(tracker.attributed_mutation_revision());
        tracker.record_exec_command_end_with_mutation_at(&[], 0, false, "", None,
            crate::turn_diff_tracker::CommandMutation::Uncertain);
        let state = SamplingRequestSettledState {
            mutation_revision: tracker.current_mutation_revision(),
            attributed_mutation_revision: tracker.attributed_mutation_revision(),
            tool_exposure_revision: 0,
        };
        assert_ne!(baseline.mutation_revision, state.mutation_revision);
        let collector = control.collector(&baseline);
        source(&collector, "same-file");
        let progress = control.observe_progress(&baseline, &collector, &state);
        assert!(!progress.contains(&TurnTimingProgressKind::WorkspaceMutation));
        if index > 0 { assert!(progress.is_empty()); }
        let decision = control.evaluate_convergence(&baseline, &collector, &state);
        assert_eq!(decision.directive.is_some(), index > 0);
    }
    let mut baseline = control.baselines(tracker.current_mutation_revision());
    baseline.set_attributed_mutation_revision(tracker.attributed_mutation_revision());
    tracker.record_exec_command_end_with_mutation_at(&[], 0, false, "", None,
        crate::turn_diff_tracker::CommandMutation::KnownMutation {
            paths: Some([std::path::PathBuf::from("changed.rs")].into_iter().collect()),
        });
    let state = SamplingRequestSettledState {
        mutation_revision: tracker.current_mutation_revision(),
        attributed_mutation_revision: tracker.attributed_mutation_revision(),
        tool_exposure_revision: 0,
    };
    let collector = control.collector(&baseline);
    assert!(control.observe_progress(&baseline, &collector, &state)
        .contains(&TurnTimingProgressKind::WorkspaceMutation));
    assert!(control.evaluate_convergence(&baseline, &collector, &state).directive.is_none());
}

#[test]
fn monitoring_exemption_requires_a_complete_monitoring_only_generation() {
    for yielded in [false, true] {
        for unrelated in ["none", "failure", "success", "missing"] {
            let control = TurnExecutionControl::new();
            let collector = control.collector(&control.baselines(0));
            record(
                &collector,
                if yielded {
                    "exec_command"
                } else {
                    "write_stdin"
                },
                r#"{"session_id":7,"chars":""}"#,
                if yielded {
                    ToolOutputOutcome::Yielded
                } else {
                    ToolOutputOutcome::Success
                },
                Some(json!({"process_observation_progress": true})),
            );
            match unrelated {
                "failure" => record(
                    &collector,
                    "update_plan",
                    "{}",
                    ToolOutputOutcome::Failure,
                    Some(json!({"failure_signature": "invalid-plan"})),
                ),
                "success" => source(&collector, "already-read.txt"),
                "missing" => {
                    collector.register_deterministic_tool_call(
                        &ToolName::plain("read_file"),
                        &payload(r#"{"path":"missing-result.txt"}"#),
                        "unfinished",
                    );
                }
                _ => {}
            }
            assert_eq!(
                collector.observed_successful_process_monitor()
                    || collector.observed_yielded_execution(),
                unrelated == "none",
                "{yielded} {unrelated}",
            );
        }
    }
}

#[test]
fn nested_poll_does_not_exempt_unrelated_direct_work() {
    for direct in [false, true] {
        let control = TurnExecutionControl::new();
        let collector = control.collector(&control.baselines(0));
        let outer = collector.register_deterministic_tool_call(
            &ToolName::plain("exec"),
            &ToolPayload::Custom {
                input: "await tools.write_stdin({session_id:7})".into(),
            },
            "outer",
        );
        collector.record_code_mode_parent("cell", Some("outer"));
        collector.record_code_mode_result(CodeModeToolResult {
            cell_id: "cell",
            tool_name: &ToolName::plain("write_stdin"),
            payload: &payload(r#"{"session_id":7}"#),
            source_dependencies: None,
            outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
            signal: Some(&json!({"process_observation_progress": true})),
            result: &json!({"session_id":7}),
            canonical_artifact_required: false,
        });
        collector.record_response_result(
            outer.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &response("nested poll"),
            false,
        );
        if direct {
            source(&collector, "old-evidence");
        }
        assert_eq!(collector.observed_successful_process_monitor(), !direct);
        assert_eq!(collector.observed_yielded_execution(), !direct);
    }
}

#[test]
fn unknown_and_empty_cycles_preserve_soft_advisory_accounting() {
    let mut control = TurnExecutionControl::new();
    let baselines = control.baselines(0);
    for index in 0..SOFT_CONVERGENCE_NO_PROGRESS_GENERATIONS {
        let collector = control.collector(&baselines);
        if index % 2 == 1 {
            collector.register_tool_call(); // An unclassified/incomplete observation.
        }
        assert!(!control.observe_budget_progress(&baselines, &collector, &settled(0)));
        let decision = control.evaluate_convergence(&baselines, &collector, &settled(0));
        assert_eq!(decision, SamplingConvergenceDecision::default());
        if index + 1 < SOFT_CONVERGENCE_NO_PROGRESS_GENERATIONS {
            assert!(control.take_soft_convergence_directive(true).is_none());
        }
    }
    assert_eq!(
        control.take_soft_convergence_directive(true).as_deref(),
        Some(SOFT_CONVERGENCE_DIRECTIVE),
    );
}

#[test]
fn alternating_cycles_converge_but_new_evidence_and_state_do_not() {
    let mut control = TurnExecutionControl::new();
    let baselines = control.baselines(0);
    for (name, repeats) in [
        ("a", false),
        ("b", false),
        ("a", true),
        ("b", true),
        ("c", false),
    ] {
        let collector = control.collector(&baselines);
        source(&collector, name);
        let decision = control.evaluate_convergence(&baselines, &collector, &settled(0));
        assert_eq!(decision.directive.is_some(), repeats, "{name}");
        assert_eq!(
            decision.continuation,
            ContinuationDisposition::ModelRequired
        );
    }
    let changed = control.baselines(1);
    let collector = control.collector(&changed);
    source(&collector, "a");
    assert!(
        control
            .evaluate_convergence(&changed, &collector, &settled(1))
            .directive
            .is_none()
    );
}

#[test]
fn cycle_history_is_bounded_and_unknown_cycles_do_not_erase_it() {
    let mut control = TurnExecutionControl::new();
    let baselines = control.baselines(0);
    for index in 0..=RECENT_CYCLE_LIMIT {
        let collector = control.collector(&baselines);
        source(&collector, &index.to_string());
        assert!(
            control
                .evaluate_convergence(&baselines, &collector, &settled(0))
                .directive
                .is_none()
        );
    }
    assert_eq!(control.recent_cycles.len(), RECENT_CYCLE_LIMIT);
    let empty = control.collector(&baselines);
    assert!(
        control
            .evaluate_convergence(&baselines, &empty, &settled(0))
            .directive
            .is_none()
    );
    for (name, repeats) in [("0", false), ("2", true)] {
        let collector = control.collector(&baselines);
        source(&collector, name);
        assert_eq!(
            control
                .evaluate_convergence(&baselines, &collector, &settled(0))
                .directive
                .is_some(),
            repeats,
        );
    }
}

#[test]
fn nested_failure_diagnosis_ignores_wrapper_but_never_suppresses_it() {
    let mut control = TurnExecutionControl::new();
    let baselines = control.baselines(0);
    let mut first_key = None;
    for (script, arguments, repeats) in [
        ("await tools.update_plan({});", "{}", false),
        ("console.log(await tools.update_plan({}));", "{}", true),
        (
            "await tools.update_plan({plan: []});",
            r#"{"plan":[]}"#,
            false,
        ),
    ] {
        let collector = control.collector(&baselines);
        let outer = collector.register_deterministic_tool_call(
            &ToolName::plain("exec"),
            &ToolPayload::Custom {
                input: script.into(),
            },
            "outer",
        );
        assert!(outer.suppressed_failure.is_none());
        collector.record_code_mode_parent("cell", Some("outer"));
        collector.record_code_mode_result(CodeModeToolResult {
            cell_id: "cell",
            tool_name: &ToolName::plain("update_plan"),
            payload: &payload(arguments),
            source_dependencies: None,
            outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            signal: Some(&json!({"failure_signature":"bad-plan", "retryable":false})),
            result: &json!({"error":"bad plan"}),
            canonical_artifact_required: false,
        });
        collector.record_response_result(
            outer.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &response("caught failure"),
            false,
        );
        let cycle = collector.deterministic_cycle().unwrap();
        assert!(cycle.repeated_failure.is_none());
        assert_eq!(collector.failure_fingerprint().as_deref(), Some("bad-plan"));
        if repeats {
            assert_eq!(first_key.as_ref(), Some(&cycle.key));
        } else if first_key.is_none() {
            first_key = Some(cycle.key.clone());
        } else {
            assert_ne!(first_key.as_ref(), Some(&cycle.key));
        }
        let decision = control.evaluate_convergence(&baselines, &collector, &settled(0));
        if repeats {
            assert!(
                decision
                    .directive
                    .as_deref()
                    .is_some_and(|text| text.contains("Convergence"))
            );
        }
        assert_eq!(
            decision.continuation,
            ContinuationDisposition::ModelRequired
        );
        assert!(
            control
                .dispatch_ledger
                .lock()
                .unwrap()
                .repeated_failure_gate
                .is_none()
        );
    }
}

#[test]
fn malformed_json_has_a_diagnostic_identity_only() {
    let mut control = TurnExecutionControl::new();
    let baselines = control.baselines(0);
    let mut first_key = None;
    for arguments in ["{", "{", "[", r#""{""#] {
        let collector = control.collector(&baselines);
        let call = collector.register_deterministic_tool_call(
            &ToolName::plain("wait"),
            &payload(arguments),
            "bad-json",
        );
        assert!(call.replayed_success.is_none());
        assert!(call.blocked_wait_guard.is_none());
        collector.record_failure(call.ordinal, "failed to parse arguments", false);
        let cycle = collector
            .deterministic_cycle()
            .expect("argument-error identity");
        assert!(cycle.repeated_failure.is_none());
        if first_key.is_none() {
            first_key = Some(cycle.key.clone());
        } else {
            assert_eq!(first_key.as_ref() == Some(&cycle.key), arguments == "{");
        }
        control.evaluate_convergence(&baselines, &collector, &settled(0));
    }
    let collector = control.collector(&baselines);
    record(
        &collector,
        "read_tool_output",
        "{",
        ToolOutputOutcome::Success,
        None,
    );
    assert!(collector.deterministic_cycle().is_none());
    assert!(collector.successful_replay_candidates().is_empty());
}

#[test]
fn settled_analysis_shares_cycles_and_unknown_results_between_consumers() {
    for kind in ["source", "failure", "unknown", "empty"] {
        let mut control = TurnExecutionControl::new();
        let baselines = control.baselines(0);
        let collector = control.collector(&baselines);
        match kind {
            "source" => source(&collector, "file"),
            "failure" => record(
                &collector,
                "update_plan",
                "{}",
                ToolOutputOutcome::Failure,
                Some(json!({"failure_signature": "bad-plan"})),
            ),
            "unknown" => { collector.register_tool_call(); }
            _ => {}
        }
        let expected = collector.deterministic_cycle();
        let analysis = collector.analyze_settled_request();
        assert!(analysis.cycle.get().is_none());
        control.evaluate_convergence_with_analysis(&baselines, &analysis, &settled(0));
        assert_eq!(analysis.cycle.get(), Some(&expected));
        let request = control.continuation_generation_request_with_analysis(
            &baselines, &analysis, &settled(0), false,
        );
        assert_eq!(request.failure_fingerprint,
            expected.as_ref().and_then(|cycle| cycle.failure_fingerprint.clone()));
        assert_eq!(analysis.cycle.get(), Some(&expected));
        drop(analysis);

        // A later settlement must not inherit this analysis, even if the same
        // collector receives additional observations.
        source(&collector, "later-file");
        let later = collector.analyze_settled_request();
        assert!(later.cycle.get().is_none());
        assert_eq!(later.deterministic_cycle().cloned(), collector.deterministic_cycle());
    }
}

#[test]
fn settled_analysis_stays_lazy_when_convergence_returns_before_cycle_analysis() {
    let mut control = TurnExecutionControl::new();
    let baselines = control.baselines(0);
    let collector = control.collector(&baselines);
    source(&collector, "file");
    let analysis = collector.analyze_settled_request();
    control.evaluate_convergence_with_analysis(&baselines, &analysis, &settled(1));
    assert!(analysis.cycle.get().is_none());
    control.continuation_generation_request_with_analysis(
        &baselines, &analysis, &settled(1), false,
    );
    assert_eq!(analysis.cycle.get(), Some(&collector.deterministic_cycle()));
}

#[test]
fn evidence_digest_is_reused_until_observed_evidence_changes() {
    let mut control = TurnExecutionControl::new();
    assert!(control.evidence_fingerprint.get().is_none());
    let initial = control.baselines(0);
    let initial_identity = initial.relevant_state_fingerprint();
    for failed in [false, true] {
        let baselines = control.baselines(0);
        let collector = control.collector(&baselines);
        if failed {
            record(&collector, "update_plan", "{}", ToolOutputOutcome::Failure,
                Some(json!({"failure_signature": "bad-plan"})));
        } else {
            source(&collector, "file");
        }
        control.observe_progress(&baselines, &collector, &settled(0));
        assert!(control.evidence_fingerprint.get().is_none());
        let changed = control.baselines(0);
        assert_ne!(baselines.evidence_fingerprint, changed.evidence_fingerprint);
        let expected = format!("{:x}", Sha256::digest(serde_json::to_vec(&(
            &control.budget_progress_evidence, &control.delivered_coverage,
        )).unwrap()));
        assert_eq!(changed.evidence_fingerprint, expected);
        assert_eq!(control.evidence_fingerprint.get(), Some(&expected));
        control.observe_progress(&baselines, &collector, &settled(0));
        assert_eq!(control.evidence_fingerprint.get(), Some(&expected));
        let revised = control.baselines_with_tool_exposure_revision(1, 2);
        assert_eq!(revised.evidence_fingerprint, expected);
        assert_ne!(revised.relevant_state_fingerprint(), changed.relevant_state_fingerprint());
        control.accepted_user_input();
        assert_eq!(control.evidence_fingerprint.get(), Some(&expected));
        assert_ne!(control.baselines(0).relevant_state_fingerprint(), changed.relevant_state_fingerprint());
    }
    assert_eq!(initial.relevant_state_fingerprint(), initial_identity);
}

#[test]
fn evidence_digest_invalidates_for_alias_coverage_even_without_novel_bytes() {
    let mut control = TurnExecutionControl::new();
    let evidence = json!({"source": "read_file", "scope": "file", "identity": {
        "sha256": "hash", "ranges": [[0, 4]], "values": []
    }});
    assert_eq!(control.observe_delivered_coverage(&evidence, None), Some(true));
    let before = control.baselines(0);
    assert_eq!(control.observe_delivered_coverage(&evidence, Some("artifact")), Some(false));
    assert!(control.evidence_fingerprint.get().is_none());
    let aliased = control.baselines(0);
    assert_ne!(before.evidence_fingerprint, aliased.evidence_fingerprint);
    let recovery = json!({"source": "artifact", "scope": "artifact", "identity": {
        "sha256": "hash", "ranges": [[4, 8]], "values": []
    }});
    assert_eq!(control.observe_delivered_coverage(&recovery, None), Some(true));
    assert!(control.evidence_fingerprint.get().is_none());
    assert_ne!(control.baselines(0).evidence_fingerprint, aliased.evidence_fingerprint);
    let covered = json!({"source": "read_file", "scope": "file", "identity": {
        "sha256": "hash", "ranges": [[0, 8]], "values": []
    }});
    assert_eq!(control.observe_delivered_coverage(&covered, None), Some(false));
}
