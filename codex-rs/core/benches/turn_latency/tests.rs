// Direct contract tests for the turn-latency benchmark.
//
// Included into `turn_latency.rs::tests` to retain access to private helpers.

use super::*;

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[test]
fn benchmark_cli_rejects_test_harness_only_command() {
    assert!(parse_command_from(strings(&["ab_overlay_"])).is_err());
}

#[test]
fn benchmark_rollback_retries_the_turn_in_progress_error() {
    let event = EventMsg::Error(codex_protocol::protocol::ErrorEvent {
        message: TURN_IN_PROGRESS_ROLLBACK_ERROR.to_string(),
        codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
    });

    assert_eq!(
        benchmark_rollback_event_action(&event).expect("turn-in-progress rollback is retryable"),
        BenchmarkRollbackEventAction::Retry
    );
}

#[test]
fn benchmark_rollback_fails_other_errors_immediately() {
    let event = EventMsg::Error(codex_protocol::protocol::ErrorEvent {
        message: "thread rollback requires persisted thread history".to_string(),
        codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
    });

    let error = benchmark_rollback_event_action(&event)
        .expect_err("non-transient rollback failures must not wait for the deadline");
    assert!(
        error
            .to_string()
            .contains("thread rollback requires persisted thread history")
    );
}

#[test]
fn ab_fixtures_pin_the_complete_baseline_reasoning_policy() {
    assert_eq!(
        ab_reasoning_phase_efforts(),
        ReasoningPhaseEfforts {
            orient: Some(ReasoningEffort::High),
            inspect: Some(ReasoningEffort::Low),
            implement: Some(ReasoningEffort::High),
            diagnose: Some(ReasoningEffort::High),
            verify: Some(ReasoningEffort::Low),
            finalize: Some(ReasoningEffort::Low),
            deterministic_continuation: Some(ReasoningEffort::Low),
        }
    );
    let legacy_fixture =
        format!("prompt=legacy_nested_dispatch\nsource={CODE_MODE_NESTED_DISPATCH_SOURCE}");
    let policy_fixture_hash = sha256_bytes(
        format!("reasoning_phase_efforts={AB_REASONING_PHASE_EFFORTS_ID}\n{legacy_fixture}")
            .as_bytes(),
    );
    assert_eq!(
        ab_fixture_hash(AbWorkload::CodeModeNestedDispatch),
        policy_fixture_hash
    );
    assert_ne!(
        ab_fixture_hash(AbWorkload::CodeModeNestedDispatch),
        sha256_bytes(legacy_fixture.as_bytes())
    );
}

fn valid_session_replay_sample(action_first: bool, lane_ns: u64) -> Sample {
    let (direct_count, nested_count) = if action_first { (10, 6) } else { (19, 16) };
    let retained_exec_index = direct_count - 4;
    let retained_poll_one_index = direct_count - 3;
    let retained_poll_two_index = direct_count - 2;
    let abort_outer_index = direct_count - 1;
    let mut tool_call_graph = (0..direct_count)
        .map(|index| AbToolGraphCallCompat {
            call_id: format!("direct-{index}"),
            execution_id: format!("direct-execution-{index}"),
            tool_name: if index == retained_exec_index {
                "exec_command"
            } else if index == retained_poll_one_index || index == retained_poll_two_index {
                "write_stdin"
            } else {
                "exec"
            }
            .to_string(),
            source: Some("direct".to_string()),
            parent_call_id: None,
            sampling_generation_id: Some(format!("generation-{index}")),
            workload_generation_index: Some(index as u32),
        })
        .collect::<Vec<_>>();
    tool_call_graph.extend((0..nested_count - 1).map(|index| AbToolGraphCallCompat {
        call_id: format!("nested-{index}"),
        execution_id: format!("nested-execution-{index}"),
        tool_name: "exec_command".to_string(),
        source: Some("code_mode".to_string()),
        parent_call_id: Some(format!("direct-{}", index % retained_exec_index.max(1))),
        sampling_generation_id: Some(format!("generation-{}", index % retained_exec_index.max(1))),
        workload_generation_index: Some((index % retained_exec_index.max(1)) as u32),
    }));
    tool_call_graph.push(AbToolGraphCallCompat {
        call_id: "nested-abort".to_string(),
        execution_id: "nested-abort-execution".to_string(),
        tool_name: "request_permissions".to_string(),
        source: Some("code_mode".to_string()),
        parent_call_id: Some(format!("direct-{abort_outer_index}")),
        sampling_generation_id: Some(format!("generation-{abort_outer_index}")),
        workload_generation_index: Some(abort_outer_index as u32),
    });
    let total_calls = direct_count + nested_count;
    let (logical_generations, provider_input_tokens, nonprogress_tokens, avoidable_generations) =
        if action_first {
            (AB_REPLAY_B_GENERATIONS, 2_000, 0, 0)
        } else {
            (AB_REPLAY_A_GENERATIONS, 4_000, 1_000, 10)
        };
    let generation_purposes = if action_first {
        BTreeMap::from([
            ("targeted_action".to_string(), 1),
            ("mutation".to_string(), 1),
            ("validation".to_string(), 1),
            ("final_response".to_string(), 2),
            ("recoverable_failure".to_string(), 1),
            ("failure_recovery".to_string(), 1),
            ("retained_process_start".to_string(), 1),
            ("retained_process_poll".to_string(), 2),
        ])
    } else {
        BTreeMap::from([
            ("necessary_work".to_string(), 8),
            ("broad_discovery".to_string(), 3),
            ("repeated_discovery".to_string(), 2),
            ("wait".to_string(), 1),
            ("terminal_failure".to_string(), 1),
        ])
    };
    Sample {
        duration_ns: lane_ns,
        inclusive_duration_ns: lane_ns,
        machine_duration_ns: lane_ns,
        controllable_duration_ns: lane_ns,
        orchestration_ns: 0,
        standalone_work_ns: 0,
        finalization_ns: lane_ns,
        preparation_ns: lane_ns,
        sampling_to_call_ns: lane_ns,
        post_tool_handoff_ns: lane_ns,
        parallel_gate_wait_ns: lane_ns,
        workspace_evidence_before_ns: lane_ns / 2,
        workspace_evidence_after_ns: lane_ns - lane_ns / 2,
        persistence_union_ns: lane_ns,
        logical_generations,
        provider_attempts: logical_generations,
        sampling_requests: logical_generations,
        avoidable_generations,
        provider_input_tokens,
        provider_cached_input_tokens: provider_input_tokens / 2,
        provider_total_tokens: provider_input_tokens,
        token_usage_records: logical_generations,
        prompt_schema_tokens: 100,
        repeated_unchanged_context_tokens: if action_first { 100 } else { 200 },
        between_tools_peak_input_tokens: if action_first { 100 } else { 200 },
        nonprogress_tokens,
        direct_tool_calls: direct_count as u32,
        nested_tool_calls: nested_count as u32,
        paired_tool_calls: total_calls as u32,
        tool_calls: total_calls as u32,
        workload_subturns: 3,
        replay_subturns: vec![
            AbReplaySubturnRecord {
                name: "actionable_success".to_string(),
                logical_generations: if action_first { 4 } else { 10 },
                terminal_event: "turn_complete".to_string(),
                application_result: "passed".to_string(),
                typed_error_count: 0,
                final_response_present: true,
                closure_complete: true,
                follow_up_artifact_present: false,
            },
            AbReplaySubturnRecord {
                name: "recoverable_exec_failure".to_string(),
                logical_generations: if action_first { 3 } else { 1 },
                terminal_event: if action_first {
                    "turn_complete"
                } else {
                    "error"
                }
                .to_string(),
                application_result: if action_first { "passed" } else { "failed" }.to_string(),
                typed_error_count: u32::from(!action_first),
                final_response_present: action_first,
                closure_complete: action_first,
                follow_up_artifact_present: action_first,
            },
            AbReplaySubturnRecord {
                name: "retained_process_abort".to_string(),
                logical_generations: if action_first { 3 } else { 4 },
                terminal_event: "turn_aborted".to_string(),
                application_result: "canceled".to_string(),
                typed_error_count: 0,
                final_response_present: false,
                closure_complete: true,
                follow_up_artifact_present: false,
            },
        ],
        replay_targeted_action: Some(AbReplayTargetedActionEvidence {
            action_first_instruction_observed: action_first,
            generation_index: 1,
            action: if action_first {
                "code_mode_exec_burst".to_string()
            } else {
                "broad_discovery".to_string()
            },
            exact_target: if action_first {
                "codex-rs/core/benches/turn_latency.rs".to_string()
            } else {
                String::new()
            },
            targeted: action_first,
        }),
        replay_reset: Some(AbReplayResetProof {
            before_sha256: "a".repeat(64),
            after_sha256: "a".repeat(64),
            passed: true,
        }),
        generation_purposes,
        failure_terminalized_subturns: u32::from(!action_first),
        typed_error_count: u32::from(!action_first),
        tool_call_graph,
        tool_closure: Some(AbToolClosureCompat {
            accepted_count: total_calls as u32,
            timing_paired_count: total_calls as u32,
            terminal_count: total_calls as u32,
            persisted_count: total_calls as u32,
            duplicate_call_id_count: 0,
            duplicate_acceptance_count: 0,
            duplicate_timing_count: 0,
            duplicate_persistence_count: 0,
            orphan_timing_count: 0,
            orphan_persistence_count: 0,
            overflow_count: 0,
            unresolved_calls: Vec::new(),
            orphan_calls: Vec::new(),
            complete: true,
        }),
        retained_write_stdin_poll_count: 2,
        retained_process_owned_before_abort: true,
        retained_process_count_before_abort: 1,
        retained_abort_process_id: Some("replay-process-1000".to_string()),
        retained_process_cleanup_complete: true,
        retained_abort_cancellation_observed: true,
        abort_registered_call_ids: vec![
            format!("direct-{abort_outer_index}"),
            "nested-abort".to_string(),
        ],
        timing_profile_valid: true,
        classification_complete: true,
        lifecycle_complete: true,
        token_coverage_complete: true,
        decision_coverage_complete: true,
        latency_eligible: true,
        ..Sample::default()
    }
}

fn injected_replay_timing_sample(base_ns: u64) -> Sample {
    Sample {
        controllable_duration_ns: base_ns + 1,
        preparation_ns: base_ns + 2,
        sampling_to_call_ns: base_ns + 3,
        post_tool_handoff_ns: base_ns + 4,
        parallel_gate_wait_ns: base_ns + 5,
        persistence_union_ns: base_ns + 6,
        finalization_ns: base_ns + 7,
        ..Sample::default()
    }
}

#[test]
fn tool_result_correctness_replay_merges_actual_failure_sample() {
    let action = injected_replay_timing_sample(100);
    let failure = injected_replay_timing_sample(1_000);
    let retained_abort = injected_replay_timing_sample(10_000);
    let expected = AbLatencyMetric::REPLAY
        .iter()
        .filter(|metric| metric.name() != "end_to_end")
        .map(|metric| {
            (
                metric.name(),
                metric
                    .value(&action)
                    .saturating_add(metric.value(&failure))
                    .saturating_add(metric.value(&retained_abort)),
            )
        })
        .collect::<Vec<_>>();

    let mut baseline = Some(action);
    merge_high_volume_sample(&mut baseline, failure);
    merge_high_volume_sample(&mut baseline, retained_abort);
    let baseline = baseline
        .as_ref()
        .expect("three replay turns must produce one baseline sample");
    let actual = AbLatencyMetric::REPLAY
        .iter()
        .filter(|metric| metric.name() != "end_to_end")
        .map(|metric| (metric.name(), metric.value(baseline)))
        .collect::<Vec<_>>();

    assert_eq!(
        actual, expected,
        "every gated replay timing lane must equal the exact sum of action, required-failure, and retained-abort timing"
    );
}

#[test]
fn tool_result_correctness_replay_error_waits_for_turn_complete_without_cleanup_generation() {
    let stack_size = AB_WORKER_STACK_BYTES
        .parse::<usize>()
        .expect("benchmark worker stack size must be valid");
    std::thread::Builder::new()
        .name("replay-error-terminal".to_string())
        .stack_size(stack_size)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build replay error-terminal runtime");
            runtime.block_on(
                session_replay_error_waits_for_turn_complete_without_cleanup_generation_fixture(),
            );
        })
        .expect("spawn replay error-terminal thread")
        .join()
        .expect("replay error-terminal thread must not panic");
}

async fn session_replay_error_waits_for_turn_complete_without_cleanup_generation_fixture() {
    let server = start_mock_server().await;
    let request_capture = HighVolumeRequestCapture::default();
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .and(request_capture.clone())
        .respond_with(
            wiremock::ResponseTemplate::new(500)
                .insert_header("content-type", "application/json")
                .set_body_string(
                    serde_json::json!({
                        "error": {
                            "type": "bad_request",
                            "message": "required replay exec failure"
                        }
                    })
                    .to_string(),
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build(&server)
        .await
        .expect("start replay error-terminal fixture");
    let fixture = ReplayActionFixture {
        _server: server,
        test,
        request_capture,
        action_response_stage: Arc::new(AtomicUsize::new(0)),
        failure_response_stage: Arc::new(AtomicUsize::new(0)),
    };

    let (sample, requests, terminalized, turns) = fixture.turn(AB_REPLAY_FAILURE_PROMPT).await;

    assert!(
        terminalized,
        "the provider error must terminalize the replay subturn"
    );
    assert_eq!(turns, 1, "terminalization must not submit a cleanup turn");
    assert_eq!(requests.len(), 1, "the measured subturn has one request");
    assert_eq!(
        fixture.request_capture.request_count(),
        1,
        "no unmeasured provider request may follow terminalization"
    );
    assert_eq!(sample.terminal_event, "turn_complete");
    assert_eq!(sample.typed_error_count, 1);
    assert_eq!(sample.failure_terminalized_subturns, 1);
    assert_eq!(sample.logical_generations, 1);
    assert_eq!(sample.provider_attempts, 1);
    assert_eq!(sample.sampling_requests, 1);
    assert!(
        sample.inclusive_duration_ns > 0,
        "the sample must come from the authoritative TurnComplete timing"
    );
    assert!(
        sample.failure_codes.is_empty(),
        "{:#?}",
        sample.failure_codes
    );
}

#[test]
fn tool_result_correctness_replay_error_signal_is_not_terminal() {
    assert!(!replay_terminal_signal_is_terminal(
        ReplayTerminalSignal::Error
    ));
    assert!(replay_terminal_signal_is_terminal(
        ReplayTerminalSignal::MatchingTurnComplete
    ));
}

fn valid_session_replay_cluster() -> AbPairedCluster {
    AbPairedCluster {
        cluster: 1,
        a_first: (0..AB_REPLAY_PAIRS).map(|index| index % 2 == 0).collect(),
        a_samples: (0..AB_REPLAY_PAIRS)
            .map(|_| valid_session_replay_sample(false, 100))
            .collect(),
        b_samples: (0..AB_REPLAY_PAIRS)
            .map(|_| valid_session_replay_sample(true, 50))
            .collect(),
        a_warmup_failures: 0,
        b_warmup_failures: 0,
        a_warmup_failure_details: Vec::new(),
        b_warmup_failure_details: Vec::new(),
    }
}

fn accepted_batch_report_for_import() -> AbReport {
    let profile = AbExecutionProfile::Batch;
    let config = profile.config();
    let workloads = ab_profile_workloads(profile, &[]).expect("batch matrix");
    let workload_names = workloads
        .iter()
        .map(|workload| workload.name().to_string())
        .collect::<Vec<_>>();
    let workload_reports = workloads
        .iter()
        .map(|workload| {
            let pairs_per_cluster = config.looks_for(*workload)[0];
            let mut clusters = match workload {
                AbWorkload::CodeModeNestedDispatch => paired_clusters(100, 50),
                AbWorkload::LongHistoryNoToolInitial
                | AbWorkload::LongHistoryToolContinuation
                | AbWorkload::StableContextWarmCache
                | AbWorkload::ContextChangeInvalidation => {
                    paired_request_cache_clusters(*workload, 100, 50)
                }
                AbWorkload::SingleDirectToolCall
                | AbWorkload::ParallelSafeTripleDirect
                | AbWorkload::ExclusiveGateSerialization => {
                    paired_tool_gate_clusters(*workload, 100, 50)
                }
                AbWorkload::CodeModeHighVolume => paired_high_volume_clusters(100, 50),
                AbWorkload::RetainedExecWriteStdinLifecycle => {
                    paired_retained_exec_clusters(100, 50)
                }
                AbWorkload::AbortDirectNestedInFlight => paired_abort_direct_nested_clusters(),
                AbWorkload::AbortRetainedProcess => paired_abort_retained_process_clusters(),
                AbWorkload::SessionReplay => unreachable!("batch reports exclude replay"),
            };
            clusters.truncate(config.clusters);
            for cluster in &mut clusters {
                cluster.a_first.truncate(pairs_per_cluster);
                cluster.a_samples.truncate(pairs_per_cluster);
                cluster.b_samples.truncate(pairs_per_cluster);
            }
            let verdict = evaluate_ab_workload_with_config(
                &clusters,
                workload.class(),
                *workload,
                config,
                pairs_per_cluster,
            )
            .expect("accepted workload verdict");
            assert!(
                verdict.passed,
                "workload `{}` must produce an accepted verdict: stop_reason={:?} gates={} diagnostics={:?} violations={:?}",
                workload.name(),
                verdict.stop_reason,
                verdict.latency_gates.len(),
                verdict.latency_diagnostics,
                verdict.correctness_violations
            );
            let sequential_look = AbSequentialLook {
                pairs_per_cluster,
                total_pairs: pairs_per_cluster * config.clusters,
                ucb_quantile: config.ucb_quantile(),
                latency_gates: verdict.latency_gates,
                latency_diagnostics: verdict.latency_diagnostics,
                correctness_violations: verdict.correctness_violations,
                decision: verdict.decision,
                stop_reason: verdict.stop_reason,
                passed: verdict.passed,
            };
            let mut report = AbWorkloadReport {
                workload: workload.name().to_string(),
                workload_class: workload.class(),
                workload_shape: workload.report_shape(),
                fixture_sha256: ab_fixture_hash(*workload),
                workload_schema_sha256: ab_workload_schema_hash(*workload),
                clusters,
                latency_gates: sequential_look.latency_gates.clone(),
                latency_diagnostics: sequential_look.latency_diagnostics.clone(),
                latency_gate_mode: config.latency_gate_mode(workload.class()),
                correctness_violations: sequential_look.correctness_violations.clone(),
                status: AbRunStatus::Passed,
                stop_reason: sequential_look.stop_reason,
                cap_expired: false,
                stopped_at_pairs_per_cluster: pairs_per_cluster,
                passed: true,
                sequential_looks: vec![sequential_look],
                report_payload_sha256: String::new(),
            };
            report.report_payload_sha256 =
                sha256_bytes(&serde_json::to_vec(&report).expect("hash workload fixture"));
            report
        })
        .collect();
    let mut report = AbReport {
        schema_version: AB_REPORT_SCHEMA_VERSION,
        workload: "turn_latency_workload_matrix".to_string(),
        provenance: AbProvenance {
            baseline_commit: "baseline".to_string(),
            candidate_commit: "candidate".to_string(),
            baseline_filtered_tree: "baseline-tree".to_string(),
            candidate_filtered_tree: "candidate-tree".to_string(),
            overlay_sha256: "overlay".to_string(),
            prepared_manifest_sha256: "manifest".to_string(),
            fixture_sha256: ab_matrix_hash(&workloads, ab_fixture_hash),
            workload_schema_sha256: ab_matrix_hash(&workloads, ab_workload_schema_hash),
            baseline_worker_sha256: "a-worker".to_string(),
            candidate_worker_sha256: "b-worker".to_string(),
            baseline_host_binary_sha256: "a-host".to_string(),
            candidate_host_binary_sha256: "b-host".to_string(),
            baseline_cli_binary_sha256: "a-cli".to_string(),
            candidate_cli_binary_sha256: "b-cli".to_string(),
            rustc_version: "rustc test".to_string(),
            rust_target: "test-target".to_string(),
            profile: AB_BUILD_PROFILE.to_string(),
            execution_profile: profile,
            features: Vec::new(),
            bootstrap_seed: AB_BOOTSTRAP_SEED,
            worker_stack_bytes: AB_WORKER_STACK_BYTES.to_string(),
            warmups_per_cluster: config.warmups,
            samples_per_cluster: config.max_pairs_per_cluster(),
            clusters: config.clusters,
            sequential_looks_per_cluster: config.looks.to_vec(),
            time_cap_seconds: config.cap.as_secs(),
            profile_configuration_sha256: ab_profile_configuration_hash(config, &workloads),
            workload_schema_version: AB_WORKLOAD_SCHEMA_VERSION,
            filtered_tree_identity_version: AB_FILTERED_TREE_IDENTITY_VERSION,
            metric_gate_version: AB_METRIC_GATE_VERSION,
            replay_session_audit: None,
        },
        requested_workloads: workload_names.clone(),
        selected_workloads: workload_names,
        unstarted_workloads: Vec::new(),
        workloads: workload_reports,
        replay_candidate_contention_self_test: None,
        status: AbRunStatus::Passed,
        cap_expired: false,
        passed: true,
        report_payload_sha256: String::new(),
    };
    report.report_payload_sha256 =
        sha256_bytes(&serde_json::to_vec(&report).expect("hash import fixture"));
    report
}

fn rehash_accepted_report(report: &mut AbReport) {
    for workload in &mut report.workloads {
        workload.report_payload_sha256.clear();
        workload.report_payload_sha256 =
            sha256_bytes(&serde_json::to_vec(&workload).expect("rehash accepted workload fixture"));
    }
    report.report_payload_sha256.clear();
    report.report_payload_sha256 =
        sha256_bytes(&serde_json::to_vec(&report).expect("rehash accepted report fixture"));
}

#[test]
fn code_mode_command_has_one_fixed_configuration() {
    let command = parse_command_from(strings(&[
        "code-mode-turn",
        "--code-mode-host",
        "code-mode-host",
    ]))
    .expect("fixed code-mode command should parse");

    let BenchmarkCommand::CodeModeCapture { host } = command else {
        panic!("expected code-mode capture command");
    };
    assert_eq!(host, PathBuf::from("code-mode-host"));

    let error = parse_command_from(strings(&[
        "code-mode-turn",
        "--code-mode-host",
        "code-mode-host",
        "--mode",
        "warm",
    ]))
    .expect_err("generic mode must not be part of code-mode capture");
    assert!(error.to_string().contains("workload is fixed"));
}

#[test]
fn synthetic_scenarios_do_not_include_code_mode_capture() {
    let command = parse_command_from(strings(&["--scenario", "deterministic"]))
        .expect("synthetic command should parse");
    let BenchmarkCommand::Synthetic(args) = command else {
        panic!("expected synthetic command");
    };
    assert_eq!(args.scenario, Some(Scenario::Deterministic));

    let error = parse_command_from(strings(&["--scenario", "code-mode-turn"]))
        .expect_err("code mode must use its dedicated command");
    assert!(error.to_string().contains("unknown scenario"));
}

#[test]
fn synthetic_defaults_include_windows_executor() {
    let scenarios = default_synthetic_scenarios();
    assert!(scenarios.contains(&Scenario::WindowsExecutor));
}

#[test]
fn synthetic_accepts_explicit_windows_executor() {
    let args = parse_synthetic_args_from(strings(&["--scenario", "windows-executor"]))
        .expect("Windows executor should be selectable");
    assert_eq!(args.scenario, Some(Scenario::WindowsExecutor));
}

#[test]
fn synthetic_rejects_duplicate_identity_and_sample_count_flags() {
    for values in [
        vec!["--scenario", "deterministic", "--scenario", "persistence"],
        vec!["--mode", "cold", "--mode", "warm"],
        vec!["--iterations", "10", "--reliability"],
        vec!["--reliability", "--iterations", "10"],
        vec!["--warmups", "1", "--warmups", "2"],
        vec!["--clusters", "1", "--clusters", "2"],
        vec!["--absolute-margin-ms", "1", "--absolute-margin-ms", "2"],
        vec!["--relative-margin", "0.01", "--relative-margin", "0.02"],
    ] {
        assert!(
            parse_synthetic_args_from(strings(&values)).is_err(),
            "duplicate benchmark configuration must be rejected: {values:?}"
        );
    }
}

#[test]
fn synthetic_rejects_non_finite_and_negative_margins() {
    for values in [
        vec!["--absolute-margin-ms", "NaN"],
        vec!["--absolute-margin-ms", "inf"],
        vec!["--absolute-margin-ms", "-1"],
        vec!["--relative-margin", "NaN"],
        vec!["--relative-margin", "inf"],
        vec!["--relative-margin", "-0.01"],
    ] {
        assert!(
            parse_synthetic_args_from(strings(&values)).is_err(),
            "invalid benchmark margin must be rejected: {values:?}"
        );
    }
}

#[test]
fn code_mode_capture_report_has_no_local_ab_gate() {
    let samples = vec![Sample {
        duration_ns: 1_000_000,
        ..Sample::default()
    }];
    let report = code_mode_capture_report(vec![CodeModeClusterReport {
        cluster: 1,
        capture: summarize(&samples),
        samples,
    }]);
    let value = serde_json::to_value(report).expect("capture report should serialize");

    assert_eq!(value["schema_version"], 4);
    assert_eq!(value["scenario"], "code_mode_turn");
    assert!(value["clusters"][0].get("capture").is_some());
    for paired_field in [
        "baseline",
        "candidate",
        "non_inferiority",
        "baseline_samples",
        "candidate_samples",
    ] {
        assert!(value["clusters"][0].get(paired_field).is_none());
    }
}

#[test]
fn ready_to_sample_dispatch_gate_rejects_multi_second_stalls() {
    assert!(!ready_to_sample_dispatch_gate_passes(None));
    assert!(!ready_to_sample_dispatch_gate_passes(Some(
        5_000_000_000_u64
    )));
    assert!(ready_to_sample_dispatch_gate_passes(Some(500_000_000_u64)));
}

#[test]
fn code_mode_workload_dispatches_exactly_one_nested_tool() {
    assert_eq!(
        CODE_MODE_NESTED_DISPATCH_SOURCE
            .matches("tools.update_plan")
            .count(),
        1
    );
    assert!(CODE_MODE_NESTED_DISPATCH_SOURCE.contains("dispatched"));
    assert!(!CODE_MODE_NESTED_DISPATCH_SOURCE.contains("completed"));
}

#[tokio::test]
async fn deterministic_cache_hit_reuses_the_cached_serialization() {
    let mut state = ScenarioState::new().expect("scenario state should initialize");
    let (_, _, first_cache_hits) = deterministic_sample(Variant::Candidate, &mut state)
        .await
        .expect("cold candidate sample should succeed");
    assert_eq!(first_cache_hits, 0);

    state.serialized_schema = Some(vec![0; 7]);
    let (_, serialized_bytes, second_cache_hits) =
        deterministic_sample(Variant::Candidate, &mut state)
            .await
            .expect("warm candidate sample should succeed");

    assert_eq!(serialized_bytes, 7);
    assert_eq!(second_cache_hits, 1);
}

#[test]
fn non_empty_samples_drive_summary_and_non_inferiority_statistics() {
    let baseline = [Sample {
        duration_ns: 1_000_000,
        sampling_requests: 2,
        ..Sample::default()
    }];
    let candidate = baseline.clone();

    let summary = summarize(&baseline);
    let gate = non_inferiority(&baseline, &candidate, 0.0, 0.0);

    assert_eq!(summary.median_ms, 1.0);
    assert_eq!(summary.failure_rate, 0.0);
    assert_eq!(gate.absolute_regression_ucb_ms, 0.0);
    assert_eq!(gate.relative_regression_ucb, 0.0);
    assert!(gate.passed);
}

#[test]
fn synthetic_non_inferiority_rejects_equal_complete_failure() {
    let baseline = [Sample {
        duration_ns: 1_000_000,
        failed: true,
        ..Sample::default()
    }];
    let candidate = baseline.clone();

    let gate = non_inferiority(&baseline, &candidate, 0.0, 0.0);

    assert_eq!(gate.failure_rate_delta, 0.0);
    assert!(!gate.passed);
}

fn valid_ab_sample(duration_ns: u64) -> Sample {
    Sample {
        duration_ns,
        inclusive_duration_ns: duration_ns,
        machine_duration_ns: duration_ns,
        controllable_duration_ns: duration_ns,
        preparation_ns: duration_ns,
        sampling_to_call_ns: duration_ns,
        post_tool_handoff_ns: duration_ns,
        logical_generations: 2,
        provider_attempts: 2,
        token_usage_records: 2,
        direct_tool_calls: 1,
        nested_tool_calls: 1,
        paired_tool_calls: 2,
        output_projection_count: 2,
        tool_closure: Some(AbToolClosureCompat {
            accepted_count: 2,
            timing_paired_count: 2,
            terminal_count: 2,
            persisted_count: 2,
            duplicate_call_id_count: 0,
            duplicate_acceptance_count: 0,
            duplicate_timing_count: 0,
            duplicate_persistence_count: 0,
            orphan_timing_count: 0,
            orphan_persistence_count: 0,
            overflow_count: 0,
            unresolved_calls: Vec::new(),
            orphan_calls: Vec::new(),
            complete: true,
        }),
        sampling_requests: 2,
        tool_calls: 2,
        timing_profile_valid: true,
        classification_complete: true,
        lifecycle_complete: true,
        token_coverage_complete: true,
        decision_coverage_complete: true,
        latency_eligible: true,
        provider_input_tokens: 2_304,
        between_tools_peak_input_tokens: 1_280,
        ..Sample::default()
    }
}

fn paired_clusters(a_duration_ns: u64, b_duration_ns: u64) -> Vec<AbPairedCluster> {
    (1..=AB_CLUSTERS)
        .map(|cluster| AbPairedCluster {
            cluster,
            a_first: (0..AB_ITERATIONS)
                .map(|index| a_runs_first(cluster, index))
                .collect(),
            a_samples: vec![valid_ab_sample(a_duration_ns); AB_ITERATIONS],
            b_samples: vec![valid_ab_sample(b_duration_ns); AB_ITERATIONS],
            a_warmup_failures: 0,
            b_warmup_failures: 0,
            a_warmup_failure_details: Vec::new(),
            b_warmup_failure_details: Vec::new(),
        })
        .collect()
}

fn valid_high_volume_sample(duration_ns: u64) -> Sample {
    let mut tool_call_graph = Vec::new();
    for generation in 0..AB_HIGH_VOLUME_SUBTURNS as u32 {
        let generation_id = format!("sampling-generation-{generation}");
        let first_direct = format!("generation-{generation}-direct-1");
        let second_direct = format!("generation-{generation}-direct-2");
        for direct in [&first_direct, &second_direct] {
            tool_call_graph.push(AbToolGraphCallCompat {
                call_id: direct.clone(),
                execution_id: format!("execution-{direct}"),
                tool_name: "exec".to_string(),
                source: Some("direct".to_string()),
                parent_call_id: None,
                sampling_generation_id: Some(generation_id.clone()),
                workload_generation_index: Some(generation),
            });
        }
        for (index, parent) in [
            first_direct.as_str(),
            second_direct.as_str(),
            second_direct.as_str(),
        ]
        .into_iter()
        .enumerate()
        {
            let call_id = format!("generation-{generation}-nested-{index}");
            tool_call_graph.push(AbToolGraphCallCompat {
                call_id: call_id.clone(),
                execution_id: format!("execution-{call_id}"),
                tool_name: "update_plan".to_string(),
                source: Some("code_mode".to_string()),
                parent_call_id: Some(parent.to_string()),
                sampling_generation_id: Some(generation_id.clone()),
                workload_generation_index: Some(generation),
            });
        }
    }
    let tool_calls = (AB_HIGH_VOLUME_SUBTURNS
        * (AB_HIGH_VOLUME_DIRECT_CALLS_PER_GENERATION + AB_HIGH_VOLUME_NESTED_CALLS_PER_GENERATION))
        as u32;
    Sample {
        duration_ns,
        inclusive_duration_ns: duration_ns,
        machine_duration_ns: duration_ns,
        controllable_duration_ns: duration_ns,
        preparation_ns: duration_ns,
        // Real high-volume samples always report a non-zero pre-first-output
        // span, so the fixture must populate it too; leaving it zero made the
        // metric uncomputable here while it gates fine against live samples.
        pre_first_output_ns: duration_ns,
        sampling_to_call_ns: duration_ns,
        logical_generations: (AB_HIGH_VOLUME_SUBTURNS * 2) as u32,
        provider_attempts: (AB_HIGH_VOLUME_SUBTURNS * 2) as u32,
        token_usage_records: (AB_HIGH_VOLUME_SUBTURNS * 2) as u32,
        provider_input_tokens: (AB_HIGH_VOLUME_SUBTURNS as u64) * (1_024 + 1_280),
        provider_cached_input_tokens: (AB_HIGH_VOLUME_SUBTURNS as u64) * (768 + 1_024),
        provider_visible_output_tokens: (AB_HIGH_VOLUME_SUBTURNS as u64) * (48 + 8),
        provider_reasoning_tokens: (AB_HIGH_VOLUME_SUBTURNS as u64) * 16,
        provider_total_tokens: (AB_HIGH_VOLUME_SUBTURNS as u64) * (1_088 + 1_288),
        between_tools_peak_input_tokens: 1_280,
        direct_tool_calls: (AB_HIGH_VOLUME_SUBTURNS * AB_HIGH_VOLUME_DIRECT_CALLS_PER_GENERATION)
            as u32,
        nested_tool_calls: (AB_HIGH_VOLUME_SUBTURNS * AB_HIGH_VOLUME_NESTED_CALLS_PER_GENERATION)
            as u32,
        paired_tool_calls: tool_calls,
        output_projection_count: tool_calls,
        workload_subturns: AB_HIGH_VOLUME_SUBTURNS as u32,
        failure_terminalized_subturns: 0,
        tool_call_graph,
        tool_closure: Some(AbToolClosureCompat {
            accepted_count: tool_calls,
            timing_paired_count: tool_calls,
            terminal_count: tool_calls,
            persisted_count: tool_calls,
            duplicate_call_id_count: 0,
            duplicate_acceptance_count: 0,
            duplicate_timing_count: 0,
            duplicate_persistence_count: 0,
            orphan_timing_count: 0,
            orphan_persistence_count: 0,
            overflow_count: 0,
            unresolved_calls: Vec::new(),
            orphan_calls: Vec::new(),
            complete: true,
        }),
        max_concurrent_tool_calls: 2,
        sampling_requests: (AB_HIGH_VOLUME_SUBTURNS * 2) as u32,
        tool_calls,
        timing_profile_valid: true,
        classification_complete: true,
        lifecycle_complete: true,
        token_coverage_complete: true,
        decision_coverage_complete: true,
        latency_eligible: true,
        ..Sample::default()
    }
}

fn paired_high_volume_clusters(a_duration_ns: u64, b_duration_ns: u64) -> Vec<AbPairedCluster> {
    (1..=AB_CLUSTERS)
        .map(|cluster| AbPairedCluster {
            cluster,
            a_first: (0..AB_ITERATIONS)
                .map(|index| a_runs_first(cluster, index))
                .collect(),
            a_samples: vec![valid_high_volume_sample(a_duration_ns); AB_ITERATIONS],
            b_samples: vec![valid_high_volume_sample(b_duration_ns); AB_ITERATIONS],
            a_warmup_failures: 0,
            b_warmup_failures: 0,
            a_warmup_failure_details: Vec::new(),
            b_warmup_failure_details: Vec::new(),
        })
        .collect()
}

fn request_component_fixture(
    stage: &str,
    history: &str,
    current_input: &str,
) -> AbRequestComponentSnapshot {
    AbRequestComponentSnapshot {
        stage: stage.to_string(),
        envelope_sha256: sha256_bytes(b"fixed request envelope"),
        instructions_sha256: sha256_bytes(b"fixed instructions"),
        tool_schemas_sha256: sha256_bytes(b"fixed tool schemas"),
        history_sha256: sha256_bytes(history.as_bytes()),
        current_input_sha256: sha256_bytes(current_input.as_bytes()),
        prompt_cache_key_sha256: sha256_bytes(b"fixed prompt cache key"),
    }
}

fn valid_request_cache_sample(workload: AbWorkload, duration_ns: u64, sequence: usize) -> Sample {
    let current_input = match workload {
        AbWorkload::ContextChangeInvalidation if sequence.is_multiple_of(2) => "context-alpha",
        AbWorkload::ContextChangeInvalidation => "context-beta",
        AbWorkload::LongHistoryNoToolInitial => "long-history-no-tool",
        AbWorkload::LongHistoryToolContinuation => "long-history-tool",
        AbWorkload::StableContextWarmCache => "stable-context",
        other => panic!("not a request/cache workload: {}", other.name()),
    };
    let mut request_components = vec![request_component_fixture(
        "initial",
        "fixed seeded history",
        current_input,
    )];
    let request_component_delta = match workload {
        AbWorkload::StableContextWarmCache => Some(request_component_delta(
            &request_component_fixture("initial", "fixed seeded history", current_input),
            &request_components[0],
        )),
        AbWorkload::ContextChangeInvalidation => {
            let previous_input = if sequence.is_multiple_of(2) {
                "context-beta"
            } else {
                "context-alpha"
            };
            Some(request_component_delta(
                &request_component_fixture("initial", "fixed seeded history", previous_input),
                &request_components[0],
            ))
        }
        _ => None,
    };
    if workload == AbWorkload::LongHistoryToolContinuation {
        request_components.push(request_component_fixture(
            "continuation",
            "fixed seeded history plus tool output",
            current_input,
        ));
    }
    let canonical_request_body_sha256 = request_components
        .iter()
        .map(|snapshot| {
            sha256_bytes(
                &serde_json::to_vec(snapshot).expect("request component fixture should serialize"),
            )
        })
        .collect();
    let direct_tool_calls = workload.expected_direct_tool_calls();
    let tool_call_graph = if direct_tool_calls == 1 {
        vec![AbToolGraphCallCompat {
            call_id: "request-cache-direct-call".to_string(),
            execution_id: "request-cache-execution".to_string(),
            tool_name: "update_plan".to_string(),
            source: Some("direct".to_string()),
            parent_call_id: None,
            sampling_generation_id: Some("request-cache-generation".to_string()),
            workload_generation_index: None,
        }]
    } else {
        Vec::new()
    };
    let mut sample = Sample {
        duration_ns,
        inclusive_duration_ns: duration_ns,
        machine_duration_ns: duration_ns,
        controllable_duration_ns: duration_ns,
        preparation_ns: duration_ns,
        pre_first_output_ns: duration_ns,
        sampling_to_call_ns: if direct_tool_calls == 0 {
            0
        } else {
            duration_ns
        },
        post_tool_handoff_ns: if direct_tool_calls == 0 {
            0
        } else {
            duration_ns
        },
        logical_generations: workload.expected_logical_generations(),
        provider_attempts: workload.expected_logical_generations(),
        tool_router_reuse_count: workload.expected_logical_generations(),
        tool_router_rebuild_count: 0,
        direct_tool_calls,
        paired_tool_calls: direct_tool_calls,
        output_projection_count: direct_tool_calls,
        workload_subturns: 1,
        tool_call_graph,
        request_components,
        canonical_request_body_sha256,
        request_component_delta,
        history_seed_turns_visible: AB_LONG_HISTORY_TURNS as u32,
        tool_closure: Some(AbToolClosureCompat {
            accepted_count: direct_tool_calls,
            timing_paired_count: direct_tool_calls,
            terminal_count: direct_tool_calls,
            persisted_count: direct_tool_calls,
            duplicate_call_id_count: 0,
            duplicate_acceptance_count: 0,
            duplicate_timing_count: 0,
            duplicate_persistence_count: 0,
            orphan_timing_count: 0,
            orphan_persistence_count: 0,
            overflow_count: 0,
            unresolved_calls: Vec::new(),
            orphan_calls: Vec::new(),
            complete: true,
        }),
        sampling_requests: workload.expected_logical_generations(),
        tool_calls: direct_tool_calls,
        timing_profile_valid: true,
        classification_complete: true,
        lifecycle_complete: true,
        token_coverage_complete: true,
        decision_coverage_complete: true,
        latency_eligible: true,
        ..Sample::default()
    };
    let expected = expected_token_usage(&sample, workload)
        .expect("request/cache workload must declare exact token usage");
    sample.token_usage_records = expected.records;
    sample.provider_input_tokens = expected.input;
    sample.provider_cached_input_tokens = expected.cached_input;
    sample.provider_visible_output_tokens = expected.visible_output;
    sample.provider_reasoning_tokens = expected.reasoning;
    sample.provider_total_tokens = expected.total;
    sample.between_tools_peak_input_tokens = expected.between_tools_peak_input;
    sample
}

fn paired_request_cache_clusters(
    workload: AbWorkload,
    a_duration_ns: u64,
    b_duration_ns: u64,
) -> Vec<AbPairedCluster> {
    paired_request_cache_clusters_with_pairs(workload, a_duration_ns, b_duration_ns, AB_ITERATIONS)
}

fn paired_request_cache_clusters_with_pairs(
    workload: AbWorkload,
    a_duration_ns: u64,
    b_duration_ns: u64,
    pairs_per_cluster: usize,
) -> Vec<AbPairedCluster> {
    (1..=AB_CLUSTERS)
        .map(|cluster| AbPairedCluster {
            cluster,
            a_first: (0..pairs_per_cluster)
                .map(|index| a_runs_first(cluster, index))
                .collect(),
            a_samples: (0..pairs_per_cluster)
                .map(|index| valid_request_cache_sample(workload, a_duration_ns, index))
                .collect(),
            b_samples: (0..pairs_per_cluster)
                .map(|index| valid_request_cache_sample(workload, b_duration_ns, index))
                .collect(),
            a_warmup_failures: 0,
            b_warmup_failures: 0,
            a_warmup_failure_details: Vec::new(),
            b_warmup_failure_details: Vec::new(),
        })
        .collect()
}

fn valid_tool_gate_sample(workload: AbWorkload, duration_ns: u64) -> Sample {
    let tool_names = match workload {
        AbWorkload::SingleDirectToolCall => vec!["test_sync_tool"],
        AbWorkload::ParallelSafeTripleDirect => {
            vec!["test_sync_tool", "test_sync_tool", "test_sync_tool"]
        }
        AbWorkload::ExclusiveGateSerialization => {
            vec!["exec_command", "exec_command", "test_sync_tool"]
        }
        other => panic!("not a tool-gate workload: {}", other.name()),
    };
    let tool_call_graph = tool_names
        .iter()
        .enumerate()
        .map(|(index, tool_name)| AbToolGraphCallCompat {
            call_id: format!("tool-gate-call-{index}"),
            execution_id: format!("tool-gate-execution-{index}"),
            tool_name: (*tool_name).to_string(),
            source: Some("direct".to_string()),
            parent_call_id: None,
            sampling_generation_id: Some("tool-gate-generation".to_string()),
            workload_generation_index: None,
        })
        .collect::<Vec<_>>();
    let tool_gate_calls = match workload {
        AbWorkload::SingleDirectToolCall => vec![AbToolGateCallCompat {
            call_id: "tool-gate-call-0".to_string(),
            tool_name: "test_sync_tool".to_string(),
            handler_entry_at_ms: Some(10),
            handler_exit_at_ms: Some(30),
            ..AbToolGateCallCompat::default()
        }],
        AbWorkload::ParallelSafeTripleDirect => (0..3)
            .map(|index| AbToolGateCallCompat {
                call_id: format!("tool-gate-call-{index}"),
                tool_name: "test_sync_tool".to_string(),
                handler_entry_at_ms: Some(10 + index),
                handler_exit_at_ms: Some(40 + index),
                ..AbToolGateCallCompat::default()
            })
            .collect(),
        AbWorkload::ExclusiveGateSerialization => vec![
            AbToolGateCallCompat {
                call_id: "tool-gate-call-0".to_string(),
                tool_name: "exec_command".to_string(),
                parallel_gate_waiter_depth_max: 1,
                handler_entry_at_ms: Some(10),
                handler_exit_at_ms: Some(110),
                ..AbToolGateCallCompat::default()
            },
            AbToolGateCallCompat {
                call_id: "tool-gate-call-1".to_string(),
                tool_name: "exec_command".to_string(),
                outcome: None,
                parallel_gate_wait_ns: 100_000_000,
                parallel_gate_waiter_depth_max: 1,
                handler_entry_at_ms: Some(110),
                handler_exit_at_ms: Some(210),
            },
            AbToolGateCallCompat {
                call_id: "tool-gate-call-2".to_string(),
                tool_name: "test_sync_tool".to_string(),
                parallel_gate_waiter_depth_max: 1,
                handler_entry_at_ms: Some(20),
                handler_exit_at_ms: Some(95),
                ..AbToolGateCallCompat::default()
            },
        ],
        other => panic!("not a tool-gate workload: {}", other.name()),
    };
    let direct_tool_calls = workload.expected_direct_tool_calls();
    let (parallel_gate_wait_ns, parallel_gate_waiter_depth_max, max_concurrent, convoy) =
        match workload {
            AbWorkload::SingleDirectToolCall => (0, 0, 1, 0),
            AbWorkload::ParallelSafeTripleDirect => (0, 0, 3, 0),
            AbWorkload::ExclusiveGateSerialization => (100_000_000, 1, 2, 1),
            other => panic!("not a tool-gate workload: {}", other.name()),
        };
    let mut sample = Sample {
        duration_ns,
        inclusive_duration_ns: duration_ns,
        machine_duration_ns: duration_ns,
        controllable_duration_ns: duration_ns,
        preparation_ns: duration_ns,
        pre_first_output_ns: duration_ns,
        sampling_to_call_ns: duration_ns,
        post_tool_handoff_ns: duration_ns,
        parallel_gate_wait_ns,
        parallel_gate_wait_max_ns: parallel_gate_wait_ns,
        parallel_gate_waiter_depth_max,
        max_concurrent_tool_calls: max_concurrent,
        convoy_count: convoy,
        logical_generations: workload.expected_logical_generations(),
        provider_attempts: workload.expected_logical_generations(),
        direct_tool_calls,
        paired_tool_calls: direct_tool_calls,
        output_projection_count: direct_tool_calls,
        workload_subturns: 1,
        tool_call_graph,
        tool_gate_calls,
        tool_closure: Some(AbToolClosureCompat {
            accepted_count: direct_tool_calls,
            timing_paired_count: direct_tool_calls,
            terminal_count: direct_tool_calls,
            persisted_count: direct_tool_calls,
            duplicate_call_id_count: 0,
            duplicate_acceptance_count: 0,
            duplicate_timing_count: 0,
            duplicate_persistence_count: 0,
            orphan_timing_count: 0,
            orphan_persistence_count: 0,
            overflow_count: 0,
            unresolved_calls: Vec::new(),
            orphan_calls: Vec::new(),
            complete: true,
        }),
        terminal_event: "turn_complete".to_string(),
        final_response_present: true,
        sampling_requests: workload.expected_logical_generations(),
        tool_calls: direct_tool_calls,
        timing_profile_valid: true,
        classification_complete: true,
        lifecycle_complete: true,
        token_coverage_complete: true,
        decision_coverage_complete: true,
        latency_eligible: true,
        ..Sample::default()
    };
    let expected = expected_token_usage(&sample, workload)
        .expect("tool-gate workload must declare exact token usage");
    sample.token_usage_records = expected.records;
    sample.provider_input_tokens = expected.input;
    sample.provider_cached_input_tokens = expected.cached_input;
    sample.provider_visible_output_tokens = expected.visible_output;
    sample.provider_reasoning_tokens = expected.reasoning;
    sample.provider_total_tokens = expected.total;
    sample.between_tools_peak_input_tokens = expected.between_tools_peak_input;
    sample
}

fn paired_tool_gate_clusters(
    workload: AbWorkload,
    a_duration_ns: u64,
    b_duration_ns: u64,
) -> Vec<AbPairedCluster> {
    (1..=AB_CLUSTERS)
        .map(|cluster| AbPairedCluster {
            cluster,
            a_first: (0..AB_ITERATIONS)
                .map(|index| a_runs_first(cluster, index))
                .collect(),
            a_samples: vec![valid_tool_gate_sample(workload, a_duration_ns); AB_ITERATIONS],
            b_samples: vec![valid_tool_gate_sample(workload, b_duration_ns); AB_ITERATIONS],
            a_warmup_failures: 0,
            b_warmup_failures: 0,
            a_warmup_failure_details: Vec::new(),
            b_warmup_failure_details: Vec::new(),
        })
        .collect()
}

fn valid_retained_exec_sample(duration_ns: u64) -> Sample {
    let workload = AbWorkload::RetainedExecWriteStdinLifecycle;
    let tool_call_graph = ["exec_command", "write_stdin", "write_stdin"]
        .into_iter()
        .enumerate()
        .map(|(index, tool_name)| AbToolGraphCallCompat {
            call_id: format!("retained-call-{index}"),
            execution_id: format!("retained-execution-{index}"),
            tool_name: tool_name.to_string(),
            source: Some("direct".to_string()),
            parent_call_id: None,
            sampling_generation_id: Some(format!("retained-generation-{index}")),
            workload_generation_index: None,
        })
        .collect();
    let mut sample = Sample {
        duration_ns,
        inclusive_duration_ns: duration_ns,
        machine_duration_ns: duration_ns,
        controllable_duration_ns: duration_ns,
        preparation_ns: duration_ns,
        pre_first_output_ns: duration_ns,
        sampling_to_call_ns: duration_ns,
        post_tool_handoff_ns: duration_ns,
        logical_generations: workload.expected_logical_generations(),
        provider_attempts: workload.expected_logical_generations(),
        direct_tool_calls: workload.expected_direct_tool_calls(),
        paired_tool_calls: workload.expected_direct_tool_calls(),
        output_projection_count: workload.expected_direct_tool_calls(),
        workload_subturns: 1,
        tool_call_graph,
        tool_closure: Some(AbToolClosureCompat {
            accepted_count: 3,
            timing_paired_count: 3,
            terminal_count: 3,
            persisted_count: 3,
            duplicate_call_id_count: 0,
            duplicate_acceptance_count: 0,
            duplicate_timing_count: 0,
            duplicate_persistence_count: 0,
            orphan_timing_count: 0,
            orphan_persistence_count: 0,
            overflow_count: 0,
            unresolved_calls: Vec::new(),
            orphan_calls: Vec::new(),
            complete: true,
        }),
        terminal_event: "turn_complete".to_string(),
        typed_error_count: 0,
        final_response_present: true,
        retained_write_stdin_poll_count: 2,
        retained_session_ids: vec!["1000".to_string(), "1000".to_string()],
        retained_process_exit_observed: true,
        retained_process_cleanup_complete: true,
        expected_retained_processes: 1,
        sampling_requests: workload.expected_logical_generations(),
        tool_calls: workload.expected_direct_tool_calls(),
        timing_profile_valid: true,
        classification_complete: true,
        lifecycle_complete: true,
        token_coverage_complete: true,
        decision_coverage_complete: true,
        latency_eligible: true,
        ..Sample::default()
    };
    let expected = expected_token_usage(&sample, workload)
        .expect("retained exec workload must declare exact token usage");
    sample.token_usage_records = expected.records;
    sample.provider_input_tokens = expected.input;
    sample.provider_cached_input_tokens = expected.cached_input;
    sample.provider_visible_output_tokens = expected.visible_output;
    sample.provider_reasoning_tokens = expected.reasoning;
    sample.provider_total_tokens = expected.total;
    sample.between_tools_peak_input_tokens = expected.between_tools_peak_input;
    sample
}

fn paired_retained_exec_clusters(a_duration_ns: u64, b_duration_ns: u64) -> Vec<AbPairedCluster> {
    (1..=AB_CLUSTERS)
        .map(|cluster| AbPairedCluster {
            cluster,
            a_first: (0..AB_ITERATIONS)
                .map(|index| a_runs_first(cluster, index))
                .collect(),
            a_samples: vec![valid_retained_exec_sample(a_duration_ns); AB_ITERATIONS],
            b_samples: vec![valid_retained_exec_sample(b_duration_ns); AB_ITERATIONS],
            a_warmup_failures: 0,
            b_warmup_failures: 0,
            a_warmup_failure_details: Vec::new(),
            b_warmup_failure_details: Vec::new(),
        })
        .collect()
}

fn valid_abort_direct_nested_sample() -> Sample {
    let workload = AbWorkload::AbortDirectNestedInFlight;
    let direct_call_id = "abort-direct".to_string();
    let nested_call_id = "abort-nested".to_string();
    let generation_id = "abort-generation".to_string();
    let tool_call_graph = vec![
        AbToolGraphCallCompat {
            call_id: direct_call_id.clone(),
            execution_id: "abort-direct-execution".to_string(),
            tool_name: "exec".to_string(),
            source: Some("direct".to_string()),
            parent_call_id: None,
            sampling_generation_id: Some(generation_id.clone()),
            workload_generation_index: None,
        },
        AbToolGraphCallCompat {
            call_id: nested_call_id.clone(),
            execution_id: "abort-nested-execution".to_string(),
            tool_name: "request_permissions".to_string(),
            source: Some("code_mode".to_string()),
            parent_call_id: Some(direct_call_id.clone()),
            sampling_generation_id: Some(generation_id),
            workload_generation_index: None,
        },
    ];
    let mut sample = Sample {
        duration_ns: 100,
        inclusive_duration_ns: 100,
        machine_duration_ns: 100,
        controllable_duration_ns: 100,
        logical_generations: workload.expected_logical_generations(),
        provider_attempts: workload.expected_logical_generations(),
        direct_tool_calls: workload.expected_direct_tool_calls(),
        nested_tool_calls: workload.expected_nested_tool_calls(),
        paired_tool_calls: workload
            .expected_direct_tool_calls()
            .saturating_add(workload.expected_nested_tool_calls()),
        output_projection_count: workload
            .expected_direct_tool_calls()
            .saturating_add(workload.expected_nested_tool_calls()),
        workload_subturns: 1,
        tool_call_graph,
        tool_closure: Some(AbToolClosureCompat {
            accepted_count: 2,
            timing_paired_count: 2,
            terminal_count: 2,
            persisted_count: 2,
            duplicate_call_id_count: 0,
            duplicate_acceptance_count: 0,
            duplicate_timing_count: 0,
            duplicate_persistence_count: 0,
            orphan_timing_count: 0,
            orphan_persistence_count: 0,
            overflow_count: 0,
            unresolved_calls: Vec::new(),
            orphan_calls: Vec::new(),
            complete: true,
        }),
        terminal_event: "turn_aborted".to_string(),
        abort_reason: Some("interrupted".to_string()),
        abort_registered_call_ids: vec![direct_call_id, nested_call_id.clone()],
        abort_terminal_outcomes_by_registration: vec!["failure".to_string(), "failure".to_string()],
        abort_barrier_call_id: Some(nested_call_id),
        sampling_requests: workload.expected_logical_generations(),
        tool_calls: workload
            .expected_direct_tool_calls()
            .saturating_add(workload.expected_nested_tool_calls()),
        timing_profile_valid: true,
        classification_complete: true,
        lifecycle_complete: true,
        token_coverage_complete: true,
        decision_coverage_complete: true,
        latency_eligible: false,
        ..Sample::default()
    };
    let expected = expected_token_usage(&sample, workload)
        .expect("direct+nested abort workload must declare exact token usage");
    sample.token_usage_records = expected.records;
    sample.provider_input_tokens = expected.input;
    sample.provider_cached_input_tokens = expected.cached_input;
    sample.provider_visible_output_tokens = expected.visible_output;
    sample.provider_reasoning_tokens = expected.reasoning;
    sample.provider_total_tokens = expected.total;
    sample.between_tools_peak_input_tokens = expected.between_tools_peak_input;
    sample
}

fn paired_abort_direct_nested_clusters() -> Vec<AbPairedCluster> {
    (1..=AB_CLUSTERS)
        .map(|cluster| AbPairedCluster {
            cluster,
            a_first: (0..AB_ITERATIONS)
                .map(|index| a_runs_first(cluster, index))
                .collect(),
            a_samples: vec![valid_abort_direct_nested_sample(); AB_ITERATIONS],
            b_samples: vec![valid_abort_direct_nested_sample(); AB_ITERATIONS],
            a_warmup_failures: 0,
            b_warmup_failures: 0,
            a_warmup_failure_details: Vec::new(),
            b_warmup_failure_details: Vec::new(),
        })
        .collect()
}

fn valid_abort_retained_process_sample() -> Sample {
    let workload = AbWorkload::AbortRetainedProcess;
    let call_id = "abort-retained-exec".to_string();
    let tool_call_graph = vec![AbToolGraphCallCompat {
        call_id: call_id.clone(),
        execution_id: "abort-retained-execution".to_string(),
        tool_name: "exec_command".to_string(),
        source: Some("direct".to_string()),
        parent_call_id: None,
        sampling_generation_id: Some("abort-retained-generation".to_string()),
        workload_generation_index: None,
    }];
    let mut sample = Sample {
        duration_ns: 100,
        inclusive_duration_ns: 100,
        machine_duration_ns: 100,
        controllable_duration_ns: 100,
        logical_generations: workload.expected_logical_generations(),
        provider_attempts: workload.expected_logical_generations(),
        direct_tool_calls: workload.expected_direct_tool_calls(),
        paired_tool_calls: workload.expected_direct_tool_calls(),
        output_projection_count: workload.expected_direct_tool_calls(),
        workload_subturns: 1,
        tool_call_graph,
        tool_closure: Some(AbToolClosureCompat {
            accepted_count: 1,
            timing_paired_count: 1,
            terminal_count: 1,
            persisted_count: 1,
            duplicate_call_id_count: 0,
            duplicate_acceptance_count: 0,
            duplicate_timing_count: 0,
            duplicate_persistence_count: 0,
            orphan_timing_count: 0,
            orphan_persistence_count: 0,
            overflow_count: 0,
            unresolved_calls: Vec::new(),
            orphan_calls: Vec::new(),
            complete: true,
        }),
        terminal_event: "turn_aborted".to_string(),
        abort_reason: Some("interrupted".to_string()),
        abort_registered_call_ids: vec![call_id.clone()],
        abort_terminal_outcomes_by_registration: vec!["failure".to_string()],
        abort_barrier_call_id: Some(call_id),
        retained_process_exit_observed: true,
        retained_process_cleanup_complete: true,
        retained_process_owned_before_abort: true,
        retained_process_count_before_abort: 1,
        retained_abort_process_id: Some("process-1000".to_string()),
        retained_abort_persisted_result_count: 1,
        retained_abort_cancellation_observed: true,
        sampling_requests: workload.expected_logical_generations(),
        tool_calls: workload.expected_direct_tool_calls(),
        timing_profile_valid: true,
        classification_complete: true,
        lifecycle_complete: true,
        token_coverage_complete: true,
        decision_coverage_complete: true,
        latency_eligible: false,
        ..Sample::default()
    };
    let expected = expected_token_usage(&sample, workload)
        .expect("retained-process abort workload must declare exact token usage");
    sample.token_usage_records = expected.records;
    sample.provider_input_tokens = expected.input;
    sample.provider_cached_input_tokens = expected.cached_input;
    sample.provider_visible_output_tokens = expected.visible_output;
    sample.provider_reasoning_tokens = expected.reasoning;
    sample.provider_total_tokens = expected.total;
    sample.between_tools_peak_input_tokens = expected.between_tools_peak_input;
    sample
}

fn paired_abort_retained_process_clusters() -> Vec<AbPairedCluster> {
    (1..=AB_CLUSTERS)
        .map(|cluster| AbPairedCluster {
            cluster,
            a_first: (0..AB_ITERATIONS)
                .map(|index| a_runs_first(cluster, index))
                .collect(),
            a_samples: vec![valid_abort_retained_process_sample(); AB_ITERATIONS],
            b_samples: vec![valid_abort_retained_process_sample(); AB_ITERATIONS],
            a_warmup_failures: 0,
            b_warmup_failures: 0,
            a_warmup_failure_details: Vec::new(),
            b_warmup_failure_details: Vec::new(),
        })
        .collect()
}

fn test_git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .expect("git should start");
    assert!(status.success(), "git {} should succeed", args.join(" "));
}

fn test_prepared_build(root: &Path, name: &str, marker: &str) -> (String, String, AbPreparedBuild) {
    let worktree = root.join(format!("{name}-worktree"));
    install_ab_overlay(&controller_repository_root(), &worktree).unwrap();
    fs::write(worktree.join("variant.txt"), marker).unwrap();
    test_git(&worktree, &["init", "--quiet"]);
    test_git(&worktree, &["config", "user.name", "KD4 benchmark"]);
    test_git(
        &worktree,
        &["config", "user.email", "benchmark@example.invalid"],
    );
    test_git(&worktree, &["add", "."]);
    test_git(&worktree, &["commit", "--quiet", "-m", marker]);
    let commit = git_text(&worktree, &["rev-parse", "HEAD"]).unwrap();
    let filtered_tree = canonical_filtered_tree_identity(&worktree, &commit).unwrap();

    let target = root.join(format!("{name}-target"));
    let profile_dir = target.join(AB_BUILD_PROFILE_DIR);
    let deps = profile_dir.join("deps");
    fs::create_dir_all(&deps).unwrap();
    let cli = profile_dir.join(executable_name("codex"));
    let host = profile_dir.join(executable_name("codex-code-mode-host"));
    let worker = deps.join(executable_name(&format!("turn_latency-{name}")));
    fs::write(&cli, format!("{name}-cli")).unwrap();
    fs::write(&host, format!("{name}-host")).unwrap();
    fs::write(&worker, format!("{name}-worker")).unwrap();
    let build = AbBuild {
        worktree,
        cli,
        host,
        worker,
    };
    let prepared = prepared_build(&build, &target).unwrap();
    (commit, filtered_tree, prepared)
}

#[test]
fn ab_overlay_dynamic_main_capture_and_dirty_tree_rejection() {
    let repo = tempfile::tempdir().expect("temp git repo");
    test_git(repo.path(), &["init", "--quiet"]);
    test_git(repo.path(), &["config", "user.name", "KD4 benchmark"]);
    test_git(
        repo.path(),
        &["config", "user.email", "benchmark@example.invalid"],
    );
    fs::write(repo.path().join("tracked.txt"), "A\n").expect("write fixture");
    test_git(repo.path(), &["add", "tracked.txt"]);
    test_git(repo.path(), &["commit", "--quiet", "-m", "A"]);
    test_git(repo.path(), &["branch", "-M", "main"]);
    let main_commit = git_text(repo.path(), &["rev-parse", "refs/heads/main"]).unwrap();
    test_git(repo.path(), &["checkout", "--quiet", "-b", "candidate"]);
    fs::write(repo.path().join("tracked.txt"), "B\n").expect("write candidate fixture");
    test_git(repo.path(), &["add", "tracked.txt"]);
    test_git(repo.path(), &["commit", "--quiet", "-m", "B"]);
    let candidate_commit = git_text(repo.path(), &["rev-parse", "HEAD"]).unwrap();

    let (_, commit, tree) =
        clean_main_identity(repo.path()).expect("clean local main should capture");
    assert_eq!(commit, main_commit);
    assert_ne!(commit, candidate_commit);
    assert_eq!(
        tree,
        canonical_filtered_tree_identity(repo.path(), &commit).unwrap()
    );
    assert_eq!(
        clean_repo_identity(repo.path()).unwrap().1,
        candidate_commit,
        "candidate identity must continue to resolve clean HEAD"
    );

    fs::write(repo.path().join("tracked.txt"), "dirty\n").expect("dirty fixture");
    let error = clean_main_identity(repo.path()).expect_err("dirty A must be rejected");
    assert!(error.to_string().contains("repository is dirty"));
    test_git(repo.path(), &["restore", "tracked.txt"]);
    test_git(repo.path(), &["branch", "-D", "main"]);
    assert!(
        clean_main_identity(repo.path()).is_err(),
        "baseline capture must reject repositories without a local main ref"
    );
}

#[test]
fn ab_overlay_rejects_identical_commit_or_filtered_tree() {
    assert!(validate_distinct_ab_identities("a", "ta", "a", "tb").is_err());
    assert!(validate_distinct_ab_identities("a", "tree", "b", "tree").is_err());
    validate_distinct_ab_identities("a", "ta", "b", "tb")
        .expect("distinct A/B identities should pass");
}

#[test]
fn ab_overlay_candidate_must_be_single_squashed_child_of_baseline() {
    let repo = tempfile::tempdir().expect("temp git repo");
    test_git(repo.path(), &["init", "--quiet"]);
    test_git(repo.path(), &["config", "user.name", "KD4 benchmark"]);
    test_git(
        repo.path(),
        &["config", "user.email", "benchmark@example.invalid"],
    );
    fs::write(repo.path().join("tracked.txt"), "A\n").expect("write baseline fixture");
    test_git(repo.path(), &["add", "tracked.txt"]);
    test_git(repo.path(), &["commit", "--quiet", "-m", "A"]);
    test_git(repo.path(), &["branch", "-M", "main"]);
    let baseline = git_text(repo.path(), &["rev-parse", "HEAD"]).unwrap();

    fs::write(repo.path().join("tracked.txt"), "B\n").expect("write candidate fixture");
    test_git(repo.path(), &["add", "tracked.txt"]);
    test_git(repo.path(), &["commit", "--quiet", "-m", "B"]);
    let direct_child = git_text(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    validate_squashed_candidate_parent(repo.path(), &baseline, &direct_child)
        .expect("a single squashed child of A must pass");
    test_git(repo.path(), &["branch", "direct-candidate"]);

    fs::write(repo.path().join("tracked.txt"), "C\n").expect("write extra commit fixture");
    test_git(repo.path(), &["add", "tracked.txt"]);
    test_git(repo.path(), &["commit", "--quiet", "-m", "C"]);
    let grandchild = git_text(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    assert!(
        validate_squashed_candidate_parent(repo.path(), &baseline, &grandchild).is_err(),
        "a multi-commit candidate must fail"
    );

    test_git(
        repo.path(),
        &["checkout", "--quiet", "-b", "side", &baseline],
    );
    fs::write(repo.path().join("side.txt"), "side\n").expect("write side fixture");
    test_git(repo.path(), &["add", "side.txt"]);
    test_git(repo.path(), &["commit", "--quiet", "-m", "side"]);
    test_git(repo.path(), &["checkout", "--quiet", "direct-candidate"]);
    test_git(
        repo.path(),
        &["merge", "--quiet", "--no-ff", "side", "-m", "merge"],
    );
    let merge = git_text(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    assert!(
        validate_squashed_candidate_parent(repo.path(), &baseline, &merge).is_err(),
        "a merge candidate must fail the single-parent contract"
    );
}

#[test]
fn ab_overlay_hash_is_byte_identical_for_both_variants() {
    let bytes = b"benchmark-only-overlay\n";
    let dir = tempfile::tempdir().expect("temp overlay dir");
    let a = dir.path().join("A.rs");
    let b = dir.path().join("B.rs");
    fs::write(&a, bytes).expect("write A overlay");
    fs::write(&b, bytes).expect("write B overlay");
    assert_eq!(sha256_file(&a).unwrap(), sha256_file(&b).unwrap());
    assert_eq!(sha256_file(&a).unwrap(), sha256_bytes(bytes));
}

#[test]
fn ab_overlay_install_copies_the_complete_compilation_closure() {
    let destination = tempfile::tempdir().expect("temp overlay destination");
    let expected = ab_overlay_sha256_at_repository(&controller_repository_root()).unwrap();
    assert_eq!(
        install_ab_overlay(&controller_repository_root(), destination.path()).unwrap(),
        expected
    );
    assert_eq!(
        ab_overlay_sha256_at_repository(destination.path()).unwrap(),
        expected
    );
    for path in AB_OVERLAY_REPOSITORY_PATHS {
        let path = std::str::from_utf8(path).unwrap();
        assert!(
            destination.path().join(path).is_file(),
            "overlay closure omitted {path}"
        );
    }
}

#[test]
fn ab_overlay_executable_names_use_the_target_platform_suffix() {
    assert_eq!(executable_name_for_suffix("codex", ""), "codex");
    assert_eq!(executable_name_for_suffix("codex", ".exe"), "codex.exe");
    assert_eq!(
        executable_name("codex"),
        format!("codex{}", std::env::consts::EXE_SUFFIX)
    );
}

#[test]
fn ab_overlay_filtered_identity_ignores_only_the_benchmark_overlay_closure() {
    let repo = tempfile::tempdir().expect("temp git repo");
    test_git(repo.path(), &["init", "--quiet"]);
    test_git(repo.path(), &["config", "user.name", "KD4 benchmark"]);
    test_git(
        repo.path(),
        &["config", "user.email", "benchmark@example.invalid"],
    );
    let overlay = repo.path().join("codex-rs/core/benches/turn_latency.rs");
    let overlay_module = repo
        .path()
        .join("codex-rs/core/benches/turn_latency/ab_runner.rs");
    fs::create_dir_all(overlay.parent().unwrap()).expect("create overlay path");
    fs::create_dir_all(overlay_module.parent().unwrap()).expect("create overlay module path");
    fs::write(&overlay, "overlay A\n").expect("write overlay A");
    fs::write(&overlay_module, "overlay module A\n").expect("write overlay module A");
    fs::write(repo.path().join("runtime.rs"), "runtime A\n").expect("write runtime A");
    test_git(repo.path(), &["add", "."]);
    test_git(repo.path(), &["commit", "--quiet", "-m", "A"]);
    let commit_a = git_text(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    let filtered_a = canonical_filtered_tree_identity(repo.path(), &commit_a).unwrap();

    fs::write(&overlay, "overlay B\n").expect("write overlay B");
    test_git(repo.path(), &["add", "."]);
    test_git(repo.path(), &["commit", "--quiet", "-m", "overlay-only"]);
    let commit_overlay = git_text(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    assert_eq!(
        filtered_a,
        canonical_filtered_tree_identity(repo.path(), &commit_overlay).unwrap()
    );

    fs::write(&overlay_module, "overlay module B\n").expect("write overlay module B");
    test_git(repo.path(), &["add", "."]);
    test_git(
        repo.path(),
        &["commit", "--quiet", "-m", "overlay-module-only"],
    );
    let commit_overlay_module = git_text(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    assert_eq!(
        filtered_a,
        canonical_filtered_tree_identity(repo.path(), &commit_overlay_module).unwrap()
    );

    fs::write(repo.path().join("runtime.rs"), "runtime B\n").expect("write runtime B");
    test_git(repo.path(), &["add", "."]);
    test_git(repo.path(), &["commit", "--quiet", "-m", "runtime"]);
    let commit_runtime = git_text(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    assert_ne!(
        filtered_a,
        canonical_filtered_tree_identity(repo.path(), &commit_runtime).unwrap()
    );
}

#[test]
fn ab_overlay_tool_closure_is_backward_compatible_but_required_for_b() {
    assert_eq!(
        decode_tool_closure_value(&serde_json::json!({})).unwrap(),
        None
    );
    let closure = valid_ab_sample(100).tool_closure.unwrap();
    let decoded = decode_tool_closure_value(&serde_json::json!({
        "toolClosure": closure
    }))
    .expect("current closure should decode");
    assert_eq!(decoded, Some(closure));
    assert!(
        decode_tool_closure_value(&serde_json::json!({
            "toolClosure": {"acceptedCount": "two"}
        }))
        .is_err()
    );

    let mut clusters = paired_clusters(100, 50);
    for cluster in &mut clusters {
        for sample in &mut cluster.a_samples {
            sample.tool_closure = None;
        }
    }
    assert!(
        !ab_correctness_violations(
            &clusters,
            AbWorkloadClass::Latency,
            AbWorkload::CodeModeNestedDispatch,
        )
        .iter()
        .any(|violation| violation.contains("tool_closure"))
    );
    clusters[0].b_samples[0].tool_closure = None;
    assert!(
        ab_correctness_violations(
            &clusters,
            AbWorkloadClass::Latency,
            AbWorkload::CodeModeNestedDispatch,
        )
        .iter()
        .any(|violation| violation.contains("B:tool_closure_missing"))
    );

    let mut persistence_gap = paired_clusters(100, 50);
    persistence_gap[0].b_samples[0]
        .tool_closure
        .as_mut()
        .unwrap()
        .persisted_count -= 1;
    assert!(
        ab_correctness_violations(
            &persistence_gap,
            AbWorkloadClass::Latency,
            AbWorkload::CodeModeNestedDispatch,
        )
        .iter()
        .any(|violation| violation.contains("tool_closure_mismatch")),
        "the exact persisted-result count must remain a lifecycle gate"
    );

    let mut projection_gap = paired_clusters(100, 50);
    projection_gap[0].b_samples[0].output_projection_count -= 1;
    assert!(
        ab_correctness_violations(
            &projection_gap,
            AbWorkloadClass::Latency,
            AbWorkload::CodeModeNestedDispatch,
        )
        .iter()
        .any(|violation| violation.contains("output_projection_count")),
        "every accepted tool result must have exactly one projection"
    );
}

#[test]
fn ab_overlay_baseline_warmup_failures_are_diagnostic_but_candidate_failures_block() {
    let workload = AbWorkload::CodeModeNestedDispatch;
    let mut clusters = paired_clusters(100, 50);
    clusters[0].a_warmup_failures = 1;
    clusters[0].a_warmup_failure_details = vec![AbWarmupFailure {
        warmup_index: 0,
        failure_codes: vec!["baseline_failure".to_string()],
    }];
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .all(|violation| !violation.contains("warmup_failures")),
        "baseline-only warmup failures must not reject a noninferior candidate"
    );

    clusters[0].b_warmup_failures = 1;
    clusters[0].b_warmup_failure_details = vec![AbWarmupFailure {
        warmup_index: 0,
        failure_codes: vec!["candidate_failure".to_string()],
    }];
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("warmup_failures:A=1:B=1")),
        "candidate warmup failures must remain a correctness violation"
    );

    let profile = AbExecutionProfile::Batch;
    let mut accepted = accepted_batch_report_for_import();
    let workload_report = &mut accepted.workloads[0];
    let workload = AbWorkload::parse(&workload_report.workload).expect("fixture workload");
    workload_report.clusters[0].a_warmup_failures = 1;
    workload_report.clusters[0].a_warmup_failure_details = vec![AbWarmupFailure {
        warmup_index: 0,
        failure_codes: vec!["baseline_failure".to_string()],
    }];
    workload_report.report_payload_sha256.clear();
    workload_report.report_payload_sha256 = sha256_bytes(
        &serde_json::to_vec(&workload_report).expect("rehash baseline warmup fixture"),
    );
    validate_accepted_ab_workload(workload_report, workload, profile.config())
        .expect("accepted reports may retain baseline warmup diagnostics");

    workload_report.clusters[0].b_warmup_failures = 1;
    workload_report.clusters[0].b_warmup_failure_details = vec![AbWarmupFailure {
        warmup_index: 0,
        failure_codes: vec!["candidate_failure".to_string()],
    }];
    workload_report.report_payload_sha256.clear();
    workload_report.report_payload_sha256 = sha256_bytes(
        &serde_json::to_vec(&workload_report).expect("rehash candidate warmup fixture"),
    );
    assert!(
        validate_accepted_ab_workload(workload_report, workload, profile.config()).is_err(),
        "accepted reports must reject candidate warmup failures"
    );
}

#[test]
fn ab_overlay_warmup_failure_details_retain_actionable_codes() {
    let mut coded_failure = valid_ab_sample(50);
    coded_failure.failed = true;
    coded_failure.failure_codes = vec!["tool_output_count".to_string()];
    let detail = ab_warmup_failure_detail(2, "B", AbWorkload::SingleDirectToolCall, &coded_failure)
        .expect("candidate failure should be retained");
    assert_eq!(detail.warmup_index, 2);
    assert_eq!(detail.failure_codes, ["tool_output_count"]);

    let mut uncoded_failure = valid_ab_sample(50);
    uncoded_failure.failed = true;
    let detail =
        ab_warmup_failure_detail(1, "B", AbWorkload::SingleDirectToolCall, &uncoded_failure)
            .expect("uncoded failures should remain diagnosable");
    assert_eq!(detail.failure_codes, ["failed_without_failure_code"]);

    assert!(
        ab_warmup_failure_detail(0, "A", AbWorkload::ParallelSafeTripleDirect, &coded_failure,)
            .is_none(),
        "declared baseline-only defects must remain diagnostic"
    );
}

#[test]
fn ab_overlay_rejects_stale_baseline_state_versions() {
    let state = AbBaselineState {
        schema_version: AB_BASELINE_STATE_SCHEMA_VERSION - 1,
        filtered_tree_identity_version: AB_FILTERED_TREE_IDENTITY_VERSION,
        repository: PathBuf::from("repo"),
        baseline_commit: "commit".to_string(),
        baseline_filtered_tree: "tree".to_string(),
    };
    assert!(validate_ab_baseline_state(&state).is_err());
    let legacy: AbBaselineState = serde_json::from_value(serde_json::json!({
        "schema_version": AB_BASELINE_STATE_SCHEMA_VERSION,
        "repository": "repo",
        "baseline_commit": "commit",
        "baseline_filtered_tree": "tree"
    }))
    .expect("legacy state should decode for an explicit version rejection");
    assert!(validate_ab_baseline_state(&legacy).is_err());
}

#[test]
fn ab_overlay_pair_order_alternates_deterministically() {
    for cluster in 1..=AB_CLUSTERS {
        for pair in 1..AB_ITERATIONS {
            assert_ne!(a_runs_first(cluster, pair - 1), a_runs_first(cluster, pair));
        }
    }
    assert!(a_runs_first(1, 0));
    assert!(!a_runs_first(2, 0));
}

#[test]
fn ab_overlay_hierarchical_bootstrap_preserves_pairs_and_clusters() {
    let clusters = paired_clusters(100, 50);
    let gate = hierarchical_paired_bootstrap(&clusters, AbLatencyMetric::ControllableTurn)
        .expect("valid hierarchy should bootstrap");
    assert_eq!(gate.point_median_ratio, 0.5);
    assert_eq!(gate.point_p95_ratio, 0.5);
    assert_eq!(gate.median_ratio_ucb, 0.5);
    assert_eq!(gate.p95_ratio_ucb, 0.5);
    assert_eq!(gate.pairs_per_cluster, AB_ITERATIONS);
    assert_eq!(
        gate.ucb_quantile,
        AbExecutionProfile::Final.config().ucb_quantile()
    );
    assert!(gate.passed);
}

#[test]
fn ab_overlay_incremental_gate_accepts_non_regression_and_rejects_regression() {
    let unchanged = hierarchical_paired_bootstrap(
        &paired_clusters(100, 100),
        AbLatencyMetric::ControllableTurn,
    )
    .expect("an unchanged incremental candidate should bootstrap");
    assert_eq!(unchanged.target_ratio, 1.0);
    assert_eq!(unchanged.median_ratio_ucb_limit, 1.05);
    assert_eq!(unchanged.p95_ratio_ucb_limit, 1.10);
    assert!(unchanged.passed);

    let regressed = hierarchical_paired_bootstrap(
        &paired_clusters(100, 120),
        AbLatencyMetric::ControllableTurn,
    )
    .expect("a regressed incremental candidate should remain measurable");
    assert!(!regressed.passed);
}

#[test]
fn ab_overlay_p95_ratio_gate_uses_the_baseline_duration_floor() {
    let mut below_floor = paired_clusters(1_000_000, 1_000_000);
    for cluster in &mut below_floor {
        cluster.b_samples[0].controllable_duration_ns = 2_000_000;
    }
    let below_floor =
        hierarchical_paired_bootstrap(&below_floor, AbLatencyMetric::ControllableTurn)
            .expect("a sub-floor p95 should remain measurable");
    assert_eq!(below_floor.baseline_p95_ns, 1_000_000.0);
    assert!(!below_floor.p95_ratio_ucb_gate_applied);
    assert_eq!(
        below_floor.p95_ratio_ucb_gate_min_baseline_ns,
        AB_P95_RATIO_UCB_GATE_MIN_BASELINE_NS
    );
    assert!(below_floor.p95_ratio_ucb > AB_P95_RATIO_UCB_LIMIT);
    assert!(below_floor.passed);

    let mut at_floor = paired_clusters(5_000_000, 5_000_000);
    for cluster in &mut at_floor {
        cluster.b_samples[0].controllable_duration_ns = 10_000_000;
    }
    let at_floor = hierarchical_paired_bootstrap(&at_floor, AbLatencyMetric::ControllableTurn)
        .expect("an at-floor p95 should remain measurable");
    assert_eq!(at_floor.baseline_p95_ns, 5_000_000.0);
    assert!(at_floor.p95_ratio_ucb_gate_applied);
    assert!(at_floor.p95_ratio_ucb > AB_P95_RATIO_UCB_LIMIT);
    assert!(!at_floor.passed);
}

#[test]
fn ab_overlay_final_profile_balances_independent_clusters_and_worker_repetition() {
    let config = AbExecutionProfile::Final.config();

    assert_eq!(config.warmups, 3);
    assert_eq!(config.clusters, 14);
    assert_eq!(config.looks, [10]);
    assert_eq!(config.clusters * config.max_pairs_per_cluster(), 140);
    assert_eq!(config.ucb_quantile(), 1.0 - AB_FAMILY_WISE_ALPHA);
    assert_eq!(AB_BUILD_PROFILE, "release");
    assert_eq!(AB_BUILD_PROFILE_DIR, "release");
    for args in [
        AB_CLI_BUILD_ARGS.as_slice(),
        AB_HOST_BUILD_ARGS.as_slice(),
        AB_WORKER_BUILD_ARGS.as_slice(),
    ] {
        assert!(args.contains(&AB_BUILD_PROFILE_FLAG));
    }
}

#[test]
fn ab_overlay_rejects_end_to_end_regression_when_internal_lanes_improve() {
    let mut clusters = paired_clusters(100, 50);
    for cluster in &mut clusters {
        for sample in &mut cluster.a_samples {
            sample.pre_first_output_ns = 100;
        }
        for sample in &mut cluster.b_samples {
            sample.duration_ns = 200;
            sample.pre_first_output_ns = 50;
        }
    }

    let verdict = evaluate_ab_workload(
        &clusters,
        AbWorkloadClass::Latency,
        AbWorkload::CodeModeNestedDispatch,
    )
    .expect("a complete-turn regression must remain measurable");

    assert!(verdict.correctness_violations.is_empty());
    assert_eq!(verdict.decision, AbSequentialDecision::Failed);
    assert_eq!(verdict.stop_reason, AbStopReason::LatencyClearFailure);
    let end_to_end = verdict
        .latency_gates
        .iter()
        .find(|gate| gate.metric == "end_to_end")
        .expect("the hard latency contract must include total turn duration");
    assert!(!end_to_end.passed);
    assert_eq!(end_to_end.point_median_ratio, 2.0);
    assert!(
        verdict
            .latency_gates
            .iter()
            .filter(|gate| gate.metric != "end_to_end")
            .all(|gate| gate.passed),
        "the synthetic internal timing lanes all improve"
    );
}

#[test]
fn ab_overlay_zero_or_missing_a_duration_invalidates_comparison() {
    let clusters = paired_clusters(0, 1);
    let error = hierarchical_paired_bootstrap(&clusters, AbLatencyMetric::ControllableTurn)
        .expect_err("zero A duration must invalidate the metric");
    assert!(error.to_string().contains("zero A duration"));

    let quick = AbExecutionProfile::Quick.config();
    let mut advisory = paired_request_cache_clusters_with_pairs(
        AbWorkload::LongHistoryNoToolInitial,
        0,
        1,
        quick.max_pairs_per_cluster(),
    );
    advisory.truncate(quick.clusters);
    for cluster in &mut advisory {
        cluster.a_first.truncate(quick.max_pairs_per_cluster());
        cluster.a_samples.truncate(quick.max_pairs_per_cluster());
        cluster.b_samples.truncate(quick.max_pairs_per_cluster());
    }
    let advisory = evaluate_ab_workload_with_config(
        &advisory,
        AbWorkloadClass::Latency,
        AbWorkload::LongHistoryNoToolInitial,
        quick,
        quick.max_pairs_per_cluster(),
    )
    .expect("advisory profiles must retain raw samples for invalid latency lanes");
    assert_eq!(advisory.decision, AbSequentialDecision::Passed);
    assert_eq!(advisory.stop_reason, AbStopReason::AdvisoryComplete);
    assert!(!advisory.latency_diagnostics.is_empty());

    let final_verdict = evaluate_ab_workload(
        &paired_request_cache_clusters(AbWorkload::LongHistoryNoToolInitial, 0, 1),
        AbWorkloadClass::Latency,
        AbWorkload::LongHistoryNoToolInitial,
    )
    .expect("hard profiles must report invalid latency lanes without dropping samples");
    assert_eq!(final_verdict.decision, AbSequentialDecision::Failed);
    assert_eq!(final_verdict.stop_reason, AbStopReason::LatencyInvalid);
    assert!(!final_verdict.latency_diagnostics.is_empty());
}

#[test]
fn ab_overlay_pairing_and_coverage_fail_pointwise() {
    let mut clusters = paired_clusters(100, 50);
    clusters[0].b_samples[7].unresolved_tool_calls = 1;
    clusters[0].b_samples[7].paired_tool_calls = 1;
    clusters[0].b_samples[7].latency_eligible = false;
    let violations = ab_correctness_violations(
        &clusters,
        AbWorkloadClass::Latency,
        AbWorkload::CodeModeNestedDispatch,
    );
    assert!(
        violations
            .iter()
            .any(|value| value.contains("coverage_incomplete"))
    );
    assert!(violations.iter().any(|value| value.contains("tool_graph")));
}

#[test]
fn ab_overlay_correctness_only_retains_samples_and_skips_latency_bootstrap() {
    let mut clusters = paired_clusters(0, 0);
    for cluster in &mut clusters {
        for sample in cluster.a_samples.iter_mut().chain(&mut cluster.b_samples) {
            sample.latency_eligible = false;
        }
    }
    let raw_samples = serde_json::to_vec(&clusters).expect("serialize raw correctness samples");
    for case in ["abort", "blocked", "partial", "malformed_output"] {
        let verdict = evaluate_ab_workload(
            &clusters,
            AbWorkloadClass::CorrectnessOnly,
            AbWorkload::CodeModeNestedDispatch,
        )
        .expect("correctness-only samples must not enter latency bootstrap");
        assert!(
            verdict.passed,
            "{case}: {:#?}",
            verdict.correctness_violations
        );
        assert!(verdict.latency_gates.is_empty(), "{case}");
        assert!(
            !verdict
                .correctness_violations
                .iter()
                .any(|value| value.contains("coverage_incomplete")),
            "{case}"
        );
        assert_eq!(
            serde_json::to_vec(&clusters).expect("reserialize raw correctness samples"),
            raw_samples,
            "{case}"
        );
    }

    clusters[0].b_samples[7].retry_attempts = 1;
    let verdict = evaluate_ab_workload(
        &clusters,
        AbWorkloadClass::CorrectnessOnly,
        AbWorkload::CodeModeNestedDispatch,
    )
    .expect("pointwise correctness failure must remain reportable");
    assert!(!verdict.passed);
    assert!(verdict.latency_gates.is_empty());
    assert!(
        verdict
            .correctness_violations
            .iter()
            .any(|value| value.contains("retry_attempts:B=1>A=0"))
    );
    assert!(
        !verdict
            .correctness_violations
            .iter()
            .any(|value| value.contains("coverage_incomplete"))
    );

    let mut abort = paired_abort_retained_process_clusters();
    abort[0].a_samples[0].failed = true;
    abort[0].a_samples[0].failure_codes = vec!["raw_baseline_abort_defect".to_string()];
    let violations = ab_correctness_violations(
        &abort,
        AbWorkloadClass::CorrectnessOnly,
        AbWorkload::AbortRetainedProcess,
    );
    assert!(
        !violations
            .iter()
            .any(|value| value.contains("raw_baseline_abort_defect")),
        "correctness-only A must remain raw motivating evidence: {violations:#?}"
    );
    abort[0].b_samples[0].failed = true;
    abort[0].b_samples[0].failure_codes = vec!["candidate_abort_defect".to_string()];
    let violations = ab_correctness_violations(
        &abort,
        AbWorkloadClass::CorrectnessOnly,
        AbWorkload::AbortRetainedProcess,
    );
    assert!(
        violations
            .iter()
            .any(|value| value.contains("candidate_abort_defect")),
        "candidate correctness defects must still fail: {violations:#?}"
    );
}

#[test]
fn ab_overlay_request_identity_canonicalization_preserves_semantics() {
    let mut a = serde_json::json!({
        "id": "request-a",
        "prompt_cache_key": "cache-a",
        "client_metadata": {
            "x-codex-turn-metadata": "{\"session_id\":\"11111111-1111-4111-8111-111111111111\",\"turn_started_at_unix_ms\":1111111111111}"
        },
        "input": [
            {"call_id": "call-a", "content": "same skill:111111111111111111111111 C:\\\\fixture\\.tmpAAAA 22222222-2222-4222-8222-222222222222"},
            {"call_id": "call-a", "output": "same"}
        ]
    });
    let mut b = serde_json::json!({
        "id": "request-b",
        "prompt_cache_key": "cache-b",
        "client_metadata": {
            "x-codex-turn-metadata": "{\"session_id\":\"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\",\"turn_started_at_unix_ms\":9999999999999}"
        },
        "input": [
            {"call_id": "call-b", "content": "same skill:aaaaaaaaaaaaaaaaaaaaaaaa C:\\\\fixture\\.tmpZZZZ bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"},
            {"call_id": "call-b", "output": "same"}
        ]
    });
    assert_eq!(
        canonical_prompt_input_tokens_from_body(&a),
        canonical_prompt_input_tokens_from_body(&b),
        "volatile request identities must not change canonical prompt tokens"
    );
    canonicalize_request_identities(&mut a);
    canonicalize_request_identities(&mut b);
    assert_eq!(a, b, "opaque request identities must not create A/B drift");

    b["input"][1]["output"] = serde_json::json!(
        "changed semantic output with enough additional stable text to increase prompt tokens"
    );
    assert_ne!(a, b, "semantic request changes must remain observable");
    assert!(
        canonical_prompt_input_tokens_from_body(&b) > canonical_prompt_input_tokens_from_body(&a),
        "semantic prompt growth must remain observable after canonicalization"
    );
}

#[test]
fn ab_overlay_nonprogress_tokens_are_pointwise_and_latency_is_diagnostic() {
    let workload = AbWorkload::LongHistoryNoToolInitial;
    let mut clusters = paired_request_cache_clusters(workload, 100, 50);
    clusters[0].a_samples[0].nonprogress_tokens = 10;
    clusters[0].b_samples[0].nonprogress_tokens = 10;
    clusters[0].a_samples[0].nonprogress_latency_ns = 20;
    clusters[0].b_samples[0].nonprogress_latency_ns = 20;
    assert!(
        !ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|value| value.contains("nonprogress_")),
        "required terminal-response work is allowed when B does not add to it"
    );

    clusters[0].b_samples[0].nonprogress_latency_ns += 1;
    assert!(
        !ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|value| value.contains("nonprogress_latency_ns")),
        "sub-microsecond timing jitter is diagnostic rather than pointwise correctness"
    );

    clusters[0].b_samples[0].nonprogress_tokens += 1;
    let violations = ab_correctness_violations(&clusters, workload.class(), workload);
    assert!(
        violations
            .iter()
            .any(|value| value.contains("nonprogress_tokens:B=11>A=10"))
    );
    assert!(
        !violations
            .iter()
            .any(|value| value.contains("nonprogress_latency_ns"))
    );
}

#[test]
fn ab_overlay_restored_tool_outputs_are_not_prompt_regressions() {
    for workload in [
        AbWorkload::ParallelSafeTripleDirect,
        AbWorkload::ExclusiveGateSerialization,
    ] {
        let mut clusters = paired_tool_gate_clusters(workload, 100, 50);
        let a = &mut clusters[0].a_samples[0];
        a.prompt_history_tokens = 100;
        a.prompt_injected_tokens = 200;
        a.prompt_reconciliation_residual = 300;
        let b = &mut clusters[0].b_samples[0];
        b.prompt_history_tokens = 101;
        b.prompt_injected_tokens = 201;
        b.prompt_reconciliation_residual = 301;

        let violations = ab_correctness_violations(&clusters, workload.class(), workload);
        assert!(
            !violations.iter().any(|violation| {
                violation.contains("prompt_history_tokens")
                    || violation.contains("prompt_injected_tokens")
                    || violation.contains("prompt_reconciliation_residual")
            }),
            "B's restoration of tool outputs missing from raw A must remain comparable: {violations:#?}"
        );

        clusters[0].b_samples[0].prompt_current_input_tokens = 1;
        assert!(
            ab_correctness_violations(&clusters, workload.class(), workload)
                .iter()
                .any(|violation| violation.contains("prompt_current_input_tokens:B=1>A=0")),
            "unrelated prompt growth must remain pointwise gated"
        );
    }
}

#[test]
fn ab_overlay_high_volume_workload_shape_and_parentage_are_exact() {
    let workload = AbWorkload::CodeModeHighVolume;
    assert_eq!(workload.class(), AbWorkloadClass::CorrectnessOnly);
    assert!(!workload.allows_raw_baseline_behavior());
    // Both sides run identical work, so this workload reports advisory latency
    // gates rather than going unmeasured.
    assert!(!workload.latency_metrics().is_empty());
    let shape = workload.report_shape();
    assert_eq!(shape.subturns_per_sample, 16);
    assert_eq!(shape.logical_generations_per_sample, 32);
    assert_eq!(shape.direct_outer_calls_per_generation, 2);
    assert_eq!(shape.nested_calls_per_generation, 3);
    assert_eq!(shape.nested_calls_by_outer_call, [1, 2]);
    assert_eq!(shape.direct_outer_calls_per_sample, 32);
    assert_eq!(shape.nested_calls_per_sample, 48);
    assert_eq!(
        shape.subturn_terminal_outcome,
        "successful_high_volume_continuation"
    );
    assert_eq!(
        CODE_MODE_HIGH_VOLUME_SINGLE_NESTED_SOURCE
            .matches("tools.update_plan")
            .count(),
        1
    );
    assert_eq!(
        CODE_MODE_HIGH_VOLUME_DOUBLE_NESTED_SOURCE
            .matches("tools.update_plan")
            .count(),
        2
    );
    assert!(!CODE_MODE_HIGH_VOLUME_SINGLE_NESTED_SOURCE.contains("text("));
    assert!(!CODE_MODE_HIGH_VOLUME_DOUBLE_NESTED_SOURCE.contains("text("));

    let mut clusters = paired_high_volume_clusters(100, 50);
    assert!(ab_correctness_violations(&clusters, workload.class(), workload).is_empty());
    let graph_json = serde_json::to_value(&clusters[0].b_samples[0])
        .expect("raw high-volume sample should serialize");
    assert_eq!(
        graph_json["tool_call_graph"].as_array().map(Vec::len),
        Some(80)
    );
    assert_eq!(clusters[0].a_samples[0].logical_generations, 32);
    assert_eq!(clusters[0].a_samples[0].failure_terminalized_subturns, 0);
    assert_eq!(clusters[0].b_samples[0].logical_generations, 32);
    assert_eq!(clusters[0].b_samples[0].failure_terminalized_subturns, 0);

    clusters[0].b_samples[0].convoy_count = 32;
    assert!(
        !ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("convoy_count")),
        "concurrent high-volume update_plan mutations must retain their required same-workspace serialization"
    );
    clusters[0].b_samples[0].convoy_count = 0;

    let one_pair = ab_cluster_prefixes(&clusters, 1)
        .expect("high-volume correctness gate should accept one paired prefix");
    let verdict = evaluate_ab_workload_with_config(
        &one_pair,
        workload.class(),
        workload,
        AbExecutionProfile::Final.config(),
        1,
    )
    .expect("one high-volume pair per cluster should be sufficient");
    assert!(verdict.passed);
    assert_eq!(verdict.decision, AbSequentialDecision::Passed);
    assert_eq!(verdict.stop_reason, AbStopReason::CorrectnessOnlyComplete);
    // Advisory gates are recorded so a latency regression here is visible in the
    // report, but they must not change the verdict: this workload still passes
    // on correctness alone.
    assert_eq!(
        verdict.latency_gates.len(),
        workload.latency_metrics().len(),
        "advisory gates must be recorded for the high-volume workload: diagnostics={:?}",
        verdict.latency_diagnostics
    );
    assert!(verdict.latency_diagnostics.is_empty());

    let a = &mut clusters[0].a_samples[0];
    a.prompt_input_tokens = 100;
    let b = &mut clusters[0].b_samples[0];
    b.prompt_input_tokens = 100;
    b.prompt_instruction_tokens = 1;
    b.prompt_schema_tokens = 1;
    b.prompt_history_tokens = 1;
    b.prompt_current_input_tokens = 1;
    b.prompt_repository_tokens = 1;
    b.prompt_skill_tokens = 1;
    b.prompt_injected_tokens = 1;
    b.prompt_reconciliation_residual = 1;
    b.repeated_unchanged_context_tokens = 3;
    let violations = ab_correctness_violations(&clusters, workload.class(), workload);
    assert!(
        !violations.iter().any(|violation| {
            violation.contains("prompt_") || violation.contains("repeated_unchanged_context_tokens")
        }),
        "locally estimated prompt composition must remain diagnostic for high volume: {violations:#?}"
    );

    clusters[0].b_samples[0].prompt_instruction_tokens = 0;
    clusters[0].b_samples[0].prompt_schema_tokens = 0;
    clusters[0].b_samples[0].prompt_history_tokens = 0;
    clusters[0].b_samples[0].prompt_current_input_tokens = 0;
    clusters[0].b_samples[0].prompt_repository_tokens = 0;
    clusters[0].b_samples[0].prompt_skill_tokens = 0;
    clusters[0].b_samples[0].prompt_injected_tokens = 0;
    clusters[0].b_samples[0].prompt_reconciliation_residual = 0;
    clusters[0].b_samples[0].repeated_unchanged_context_tokens = 0;
    clusters[0].b_samples[0].prompt_input_tokens += 1;
    assert!(
        !ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("prompt_input_tokens")),
        "locally estimated canonical prompt growth is diagnostic for high volume"
    );
    clusters[0].b_samples[0].provider_input_tokens += 1;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| {
                violation.contains("B:token_usage") || violation.contains("provider_input_tokens")
            }),
        "provider token growth must remain a hard correctness failure"
    );
    clusters[0].b_samples[0].provider_input_tokens -= 1;

    for call in &mut clusters[0].b_samples[0].tool_call_graph {
        call.sampling_generation_id = Some("generation-0".to_string());
    }
    assert!(
        tool_graph_matches_workload(&clusters[0].b_samples[0], workload),
        "runtime generation identities are turn-scoped and may repeat across composed subturns"
    );

    clusters[0].b_samples[0].logical_generations = 33;
    clusters[0].b_samples[0].provider_attempts = 33;
    clusters[0].b_samples[0].sampling_requests = 33;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("B:generation_graph"))
    );
    clusters[0].b_samples[0].logical_generations = 32;
    clusters[0].b_samples[0].provider_attempts = 32;
    clusters[0].b_samples[0].sampling_requests = 32;

    clusters[0].b_samples[0].avoidable_generations = 1;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("avoidable_generations:B=1>A=0"))
    );
    clusters[0].b_samples[0].avoidable_generations = 0;

    let nested = clusters[0].b_samples[0]
        .tool_call_graph
        .iter_mut()
        .find(|call| call.source.as_deref() == Some("code_mode"))
        .expect("high-volume graph should contain nested calls");
    nested.parent_call_id = Some("wrong-generation-parent".to_string());
    let violations = ab_correctness_violations(&clusters, workload.class(), workload);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("tool_graph_identity"))
    );

    let mut failed_baseline = paired_high_volume_clusters(100, 50);
    failed_baseline[0].a_samples[0].failed = true;
    failed_baseline[0].a_samples[0]
        .failure_codes
        .push("synthetic_baseline_failure".to_string());
    assert!(
        ab_correctness_violations(&failed_baseline, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("A:failed:synthetic_baseline_failure")),
        "high-volume correctness coverage must not inherit abort fixtures' raw-A exemption"
    );
}

#[test]
fn ab_overlay_request_cache_workload_matrix_is_explicit() {
    let workloads = ab_controller_workloads();
    assert_eq!(
        workloads,
        [
            AbWorkload::LongHistoryNoToolInitial,
            AbWorkload::LongHistoryToolContinuation,
            AbWorkload::StableContextWarmCache,
            AbWorkload::ContextChangeInvalidation,
            AbWorkload::SingleDirectToolCall,
            AbWorkload::ParallelSafeTripleDirect,
            AbWorkload::ExclusiveGateSerialization,
            AbWorkload::CodeModeHighVolume,
            AbWorkload::RetainedExecWriteStdinLifecycle,
            AbWorkload::AbortDirectNestedInFlight,
            AbWorkload::AbortRetainedProcess,
        ]
    );
    let fixture_hashes = workloads
        .iter()
        .map(|workload| ab_fixture_hash(*workload))
        .collect::<BTreeSet<_>>();
    let schema_hashes = workloads
        .iter()
        .map(|workload| ab_workload_schema_hash(*workload))
        .collect::<BTreeSet<_>>();
    assert_eq!(fixture_hashes.len(), workloads.len());
    assert_eq!(schema_hashes.len(), workloads.len());
    assert_eq!(ab_matrix_hash(workloads, ab_fixture_hash).len(), 64);
    assert_eq!(ab_matrix_hash(workloads, ab_workload_schema_hash).len(), 64);

    for workload in &workloads[..4] {
        let shape = workload.report_shape();
        assert_eq!(workload.class(), AbWorkloadClass::Latency);
        assert_eq!(shape.subturns_per_sample, 1);
        assert_eq!(shape.history_seed_turns, AB_LONG_HISTORY_TURNS as u32);
        assert_eq!(
            shape.logical_generations_per_sample,
            shape.model_requests_per_sample
        );
        assert!(!shape.cache_assertion.is_empty());
        assert_eq!(
            shape.latency_metrics.len(),
            workload.latency_metrics().len()
        );
        assert!(
            shape
                .latency_metrics
                .iter()
                .all(|metric| metric != "sampling_to_call")
                || *workload == AbWorkload::LongHistoryToolContinuation
        );
    }
}

#[test]
fn ab_overlay_single_direct_tool_is_exact_and_routable() {
    let workload = AbWorkload::SingleDirectToolCall;
    assert_eq!(AbWorkload::parse(workload.name()).unwrap(), workload);
    assert_eq!(workload.class(), AbWorkloadClass::Latency);
    assert_eq!(workload.expected_logical_generations(), 2);
    assert_eq!(workload.expected_direct_tool_calls(), 1);
    assert_eq!(workload.expected_nested_tool_calls(), 0);
    assert_eq!(
        workload
            .latency_metrics()
            .iter()
            .map(|metric| metric.name())
            .collect::<Vec<_>>(),
        AbLatencyMetric::ALL
            .iter()
            .map(|metric| metric.name())
            .collect::<Vec<_>>()
    );

    let shape = workload.report_shape();
    assert_eq!(shape.subturns_per_sample, 1);
    assert_eq!(shape.logical_generations_per_sample, 2);
    assert_eq!(shape.model_requests_per_sample, 2);
    assert_eq!(shape.direct_outer_calls_per_generation, 1);
    assert_eq!(shape.direct_outer_calls_per_sample, 1);
    assert_eq!(shape.nested_calls_per_sample, 0);
    assert_eq!(shape.nested_calls_by_outer_call, [0]);
    assert_eq!(
        shape.subturn_terminal_outcome,
        "successful_single_direct_continuation"
    );

    let command = parse_ab_worker_args_from(strings(&[
        "--code-mode-host",
        "host",
        "--variant",
        "B",
        "--cluster",
        "1",
        "--workload",
        workload.name(),
        "--warmups",
        "1",
        "--samples",
        "1",
    ]))
    .expect("single direct worker route should parse");
    let BenchmarkCommand::AbWorker(args) = command else {
        panic!("expected A/B worker command");
    };
    assert_eq!(args.workload, workload);

    let mut clusters = paired_tool_gate_clusters(workload, 100, 50);
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload).is_empty(),
        "valid single direct sample must satisfy exact graph and lifecycle gates"
    );
    assert!(tool_gate_execution_matches(
        &clusters[0].b_samples[0],
        workload
    ));
    clusters[0].b_samples[0].max_concurrent_tool_calls = 2;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("single_direct_tool_call_contract"))
    );
    assert_eq!(ab_fixture_hash(workload).len(), 64);
    assert_eq!(ab_workload_schema_hash(workload).len(), 64);
}

#[test]
fn ab_overlay_parallel_safe_triple_is_exact_and_concurrent() {
    let workload = AbWorkload::ParallelSafeTripleDirect;
    assert_eq!(AbWorkload::parse(workload.name()).unwrap(), workload);
    assert_eq!(workload.class(), AbWorkloadClass::CorrectnessOnly);
    assert!(workload.latency_metrics().is_empty());
    assert_eq!(workload.expected_logical_generations(), 2);
    assert_eq!(workload.expected_direct_tool_calls(), 3);
    assert_eq!(workload.expected_nested_tool_calls(), 0);
    assert!(
        !workload
            .latency_metrics()
            .iter()
            .any(|metric| matches!(metric, AbLatencyMetric::ParallelGateWait))
    );

    let shape = workload.report_shape();
    assert_eq!(shape.subturns_per_sample, 1);
    assert_eq!(shape.logical_generations_per_sample, 2);
    assert_eq!(shape.model_requests_per_sample, 2);
    assert_eq!(shape.direct_outer_calls_per_generation, 3);
    assert_eq!(shape.direct_outer_calls_per_sample, 3);
    assert_eq!(shape.nested_calls_per_sample, 0);
    assert_eq!(shape.nested_calls_by_outer_call, [0, 0, 0]);
    assert_eq!(
        shape.subturn_terminal_outcome,
        "successful_parallel_safe_continuation"
    );

    let command = parse_ab_worker_args_from(strings(&[
        "--code-mode-host",
        "host",
        "--variant",
        "A",
        "--cluster",
        "1",
        "--workload",
        workload.name(),
        "--warmups",
        "1",
        "--samples",
        "1",
    ]))
    .expect("parallel-safe worker route should parse");
    let BenchmarkCommand::AbWorker(args) = command else {
        panic!("expected A/B worker command");
    };
    assert_eq!(args.workload, workload);

    let mut clusters = paired_tool_gate_clusters(workload, 100, 50);
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload).is_empty(),
        "valid parallel-safe sample must satisfy exact concurrency and graph gates"
    );
    let sample = &clusters[0].b_samples[0];
    assert_eq!(sample.max_concurrent_tool_calls, 3);
    assert_eq!(sample.parallel_gate_waiter_depth_max, 0);
    assert_eq!(sample.convoy_count, 0);
    assert_eq!(sample.unrelated_parallel_safe_convoy_count, 0);
    assert_eq!(
        sample
            .tool_call_graph
            .iter()
            .filter_map(|call| call.sampling_generation_id.as_deref())
            .collect::<BTreeSet<_>>()
            .len(),
        1
    );
    clusters[0].b_samples[0].max_concurrent_tool_calls = 2;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("parallel_safe_triple_direct_contract"))
    );
    clusters[0].b_samples[0] = valid_tool_gate_sample(workload, 50);
    clusters[0].b_samples[0].tool_call_graph[2].sampling_generation_id =
        Some("different-generation".to_string());
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("tool_graph_identity"))
    );
    assert_eq!(ab_fixture_hash(workload).len(), 64);
    assert_eq!(ab_workload_schema_hash(workload).len(), 64);
}

#[test]
fn ab_overlay_tool_gate_retains_defective_baseline_output_counts() {
    let workload = AbWorkload::ParallelSafeTripleDirect;
    let events = tool_gate_continuation_events("defective-baseline", workload, 1);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["type"], "response.output_item.done");
    assert_eq!(events[1]["type"], "response.completed");

    let mut clusters = paired_tool_gate_clusters(workload, 100, 50);
    clusters[0].a_samples[0].failed = true;
    clusters[0].a_samples[0].failure_codes = vec!["tool_output_count".to_string()];
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload).is_empty(),
        "the declared raw A projection defect must remain measurable"
    );

    clusters[0].b_samples[0].failed = true;
    clusters[0].b_samples[0].failure_codes = vec!["tool_output_count".to_string()];
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("B:failed:tool_output_count")),
        "B must correct the declared baseline defect"
    );

    clusters[0].b_samples[0] = valid_tool_gate_sample(workload, 50);
    clusters[0].a_samples[0].failure_codes = vec!["unknown_baseline_defect".to_string()];
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("A:failed:unknown_baseline_defect")),
        "undeclared A defects must still invalidate the comparison"
    );
}

#[test]
fn ab_overlay_exclusive_gate_serializes_without_unrelated_convoy() {
    let workload = AbWorkload::ExclusiveGateSerialization;
    assert_eq!(AbWorkload::parse(workload.name()).unwrap(), workload);
    assert_eq!(workload.class(), AbWorkloadClass::Latency);
    assert_eq!(workload.expected_logical_generations(), 2);
    assert_eq!(workload.expected_direct_tool_calls(), 3);
    assert_eq!(workload.expected_nested_tool_calls(), 0);
    assert_eq!(
        workload
            .latency_metrics()
            .iter()
            .map(|metric| metric.name())
            .collect::<Vec<_>>(),
        AbLatencyMetric::WITH_PARALLEL_GATE_WAIT
            .iter()
            .map(|metric| metric.name())
            .collect::<Vec<_>>()
    );

    let shape = workload.report_shape();
    assert_eq!(shape.subturns_per_sample, 1);
    assert_eq!(shape.logical_generations_per_sample, 2);
    assert_eq!(shape.model_requests_per_sample, 2);
    assert_eq!(shape.direct_outer_calls_per_generation, 3);
    assert_eq!(shape.direct_outer_calls_per_sample, 3);
    assert_eq!(shape.nested_calls_per_sample, 0);
    assert_eq!(shape.nested_calls_by_outer_call, [0, 0, 0]);
    assert!(
        shape
            .cache_assertion
            .contains("same-resource exec calls serialize")
    );
    assert!(
        shape
            .cache_assertion
            .contains("unrelated safe call overlaps")
    );

    let command = parse_ab_worker_args_from(strings(&[
        "--code-mode-host",
        "host",
        "--variant",
        "B",
        "--cluster",
        "1",
        "--workload",
        workload.name(),
        "--warmups",
        "1",
        "--samples",
        "1",
    ]))
    .expect("exclusive-gate worker route should parse");
    let BenchmarkCommand::AbWorker(args) = command else {
        panic!("expected A/B worker command");
    };
    assert_eq!(args.workload, workload);
    assert!(matches!(
        parse_command_from(strings(&["ab-exclusive-gate-child"])).unwrap(),
        BenchmarkCommand::AbExclusiveGateChild
    ));
    assert!(parse_command_from(strings(&["ab-exclusive-gate-child", "unexpected"])).is_err());
    const {
        assert!(AB_EXCLUSIVE_GATE_YIELD_TIME_MS > AB_EXCLUSIVE_GATE_CHILD_DELAY_MS * 10);
    }

    let mut clusters = paired_tool_gate_clusters(workload, 200_000_000, 100_000_000);
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload).is_empty(),
        "valid exclusive-gate sample must prove serialization and unrelated overlap"
    );
    clusters[0].b_samples[0].tool_call_graph.swap(0, 2);
    clusters[0].b_samples[0].tool_gate_calls.swap(0, 2);
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload).is_empty(),
        "parallel completion order must not change the declared call graph or gate topology"
    );
    clusters[0].b_samples[0].tool_call_graph.swap(0, 2);
    clusters[0].b_samples[0].tool_gate_calls.swap(0, 2);
    let sample = &clusters[0].b_samples[0];
    assert_eq!(sample.max_concurrent_tool_calls, 2);
    assert_eq!(sample.parallel_gate_waiter_depth_max, 1);
    assert_eq!(sample.convoy_count, 1);
    assert_eq!(sample.unrelated_parallel_safe_convoy_count, 0);
    assert_eq!(sample.tool_gate_calls[0].tool_name, "exec_command");
    assert_eq!(sample.tool_gate_calls[1].tool_name, "exec_command");
    assert_eq!(sample.tool_gate_calls[2].tool_name, "test_sync_tool");
    assert_eq!(sample.tool_gate_calls[0].parallel_gate_wait_ns, 0);
    assert!(sample.tool_gate_calls[1].parallel_gate_wait_ns > 0);
    assert_eq!(sample.tool_gate_calls[2].parallel_gate_wait_ns, 0);
    assert!(
        sample.tool_gate_calls[0].handler_exit_at_ms
            <= sample.tool_gate_calls[1].handler_entry_at_ms
    );
    assert!(
        sample.tool_gate_calls[2].handler_entry_at_ms
            < sample.tool_gate_calls[0].handler_exit_at_ms
            && sample.tool_gate_calls[0].handler_entry_at_ms
                < sample.tool_gate_calls[2].handler_exit_at_ms
    );

    clusters[0].b_samples[0].unrelated_parallel_safe_convoy_count = 1;
    clusters[0].b_samples[0].tool_gate_calls[2].parallel_gate_wait_ns = 1;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("exclusive_gate_serialization_contract"))
    );
    clusters[0].b_samples[0] = valid_tool_gate_sample(workload, 100_000_000);
    clusters[0].b_samples[0].tool_gate_calls[2].handler_entry_at_ms = Some(111);
    clusters[0].b_samples[0].tool_gate_calls[2].handler_exit_at_ms = Some(120);
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("exclusive_gate_serialization_contract"))
    );

    let mut legacy_value =
        serde_json::to_value(valid_tool_gate_sample(workload, 100_000_000)).unwrap();
    let legacy_object = legacy_value.as_object_mut().unwrap();
    legacy_object.remove("parallel_gate_waiter_depth_max");
    legacy_object.remove("unrelated_parallel_safe_convoy_count");
    legacy_object.remove("tool_gate_calls");
    let legacy: Sample = serde_json::from_value(legacy_value).unwrap();
    assert_eq!(legacy.parallel_gate_waiter_depth_max, 0);
    assert_eq!(legacy.unrelated_parallel_safe_convoy_count, 0);
    assert!(legacy.tool_gate_calls.is_empty());
    assert_eq!(ab_fixture_hash(workload).len(), 64);
    assert_eq!(ab_workload_schema_hash(workload).len(), 64);
}

#[test]
fn ab_overlay_retained_exec_lifecycle_is_exact_and_routable() {
    let workload = AbWorkload::RetainedExecWriteStdinLifecycle;
    assert_eq!(AbWorkload::parse(workload.name()).unwrap(), workload);
    assert_eq!(workload.class(), AbWorkloadClass::Latency);
    assert_eq!(workload.expected_logical_generations(), 4);
    assert_eq!(workload.expected_direct_tool_calls(), 3);
    assert_eq!(workload.expected_nested_tool_calls(), 0);
    assert_eq!(
        workload
            .latency_metrics()
            .iter()
            .map(|metric| metric.name())
            .collect::<Vec<_>>(),
        AbLatencyMetric::ALL
            .iter()
            .map(|metric| metric.name())
            .collect::<Vec<_>>()
    );

    let shape = workload.report_shape();
    assert_eq!(shape.subturns_per_sample, 1);
    assert_eq!(shape.logical_generations_per_sample, 4);
    assert_eq!(shape.model_requests_per_sample, 4);
    assert_eq!(shape.direct_outer_calls_per_sample, 3);
    assert_eq!(shape.nested_calls_per_sample, 0);
    assert_eq!(shape.nested_calls_by_outer_call, [0, 0, 0]);
    assert_eq!(
        shape.subturn_terminal_outcome,
        "passed_after_retained_process_exit_and_cleanup"
    );
    assert_eq!(ab_fixture_hash(workload).len(), 64);
    assert_eq!(ab_workload_schema_hash(workload).len(), 64);
    assert_ne!(
        ab_fixture_hash(workload),
        ab_fixture_hash(AbWorkload::CodeModeHighVolume)
    );

    let command = parse_ab_worker_args_from(strings(&[
        "--code-mode-host",
        "host",
        "--variant",
        "B",
        "--cluster",
        "1",
        "--workload",
        "retained_exec_write_stdin_lifecycle",
        "--warmups",
        "1",
        "--samples",
        "1",
    ]))
    .expect("retained exec worker route should parse");
    let BenchmarkCommand::AbWorker(args) = command else {
        panic!("expected A/B worker command");
    };
    assert_eq!(args.workload, workload);
    assert!(matches!(
        parse_command_from(strings(&["ab-retained-child"])).unwrap(),
        BenchmarkCommand::AbRetainedChild
    ));
    assert!(parse_command_from(strings(&["ab-retained-child", "unexpected"])).is_err());
    let replay_command = parse_command_from(strings(&[
        "ab-replay-command",
        "read",
        "source_owners.toml",
        "codex-rs/core/src/codex.rs",
    ]))
    .expect("replay child command should parse");
    let BenchmarkCommand::AbReplayCommand { mode, paths } = replay_command else {
        panic!("expected replay child command");
    };
    assert_eq!(mode, "read");
    assert_eq!(
        paths,
        vec![
            PathBuf::from("source_owners.toml"),
            PathBuf::from("codex-rs/core/src/codex.rs")
        ]
    );
    assert!(parse_command_from(strings(&["ab-replay-command", "read"])).is_err());
    assert_eq!(
        retained_session_id_from_output(
            "Process running with session ID 1000\n__KD4_RETAINED_READY__\n"
        )
        .as_deref(),
        Some("1000")
    );
    assert_eq!(
        retained_session_id_from_output(
            "Process running with session ID 1001; wall time: 0.01 seconds\n"
        )
        .as_deref(),
        Some("1001")
    );
    assert!(retained_process_exit_observed(
        &[Some("__KD4_RETAINED_FINISHED__\n")],
        true
    ));
    assert!(!retained_process_exit_observed(
        &[Some("__KD4_RETAINED_FINISHED__\n")],
        false
    ));
    assert!(!retained_process_exit_observed(
        &[Some(
            "Process exited with code 0; wall time: 1.2500 seconds\n",
        )],
        true
    ));
    assert!(retained_process_exit_observed(
        &[
            Some("__KD4_RETAINED_POLL__\n__KD4_RETAINED_FINISHED__\n"),
            Some("Process exited with code 0; wall time: 1.2500 seconds\n"),
        ],
        true
    ));

    let mut coalesced_control = std::io::Cursor::new(b"poll\nfinish\n");
    let mut pending_control = Vec::new();
    wait_for_retained_control(&mut coalesced_control, &mut pending_control, b"poll")
        .expect("the live poll marker should be recognized");
    wait_for_retained_control(&mut coalesced_control, &mut pending_control, b"finish")
        .expect("the coalesced terminal marker must remain available");

    let retained_call = codex_protocol::protocol::TurnTimingToolCall {
        call_id: "retained-call-0".to_string(),
        tool_name: "exec_command".to_string(),
        source: TurnTimingToolCallSource::Direct,
        accepted_at_ms: Some(1),
        first_poll_at_ms: Some(2),
        parallel_gate_admitted_at_ms: Some(3),
        handler_entry_at_ms: Some(4),
        process_spawned_at_ms: Some(5),
        handler_exit_at_ms: Some(6),
        output_collected_at_ms: Some(7),
        delivered_at_ms: Some(8),
        output_model_visible_at_ms: Some(9),
        model_resumed_at_ms: Some(10),
        process_exited_at_ms: Some(90),
        background_process_expected: true,
        ..codex_protocol::protocol::TurnTimingToolCall::default()
    };
    assert_eq!(tool_call_lifecycle_diagnostic(&retained_call), None);

    let mut rounded_legacy_poll = retained_call.clone();
    rounded_legacy_poll.first_poll_at_ms = Some(4);
    rounded_legacy_poll.parallel_gate_admitted_at_ms = Some(3);
    assert_eq!(
        tool_call_lifecycle_diagnostic(&rounded_legacy_poll),
        None,
        "legacy reconstructed first-poll time must not be ordered against exact admission"
    );

    let mut foreground_call = retained_call.clone();
    foreground_call.background_process_expected = false;
    assert_eq!(
        tool_call_lifecycle_diagnostic(&foreground_call)
            .expect("foreground process exit after handler return must be diagnosed")
            .nonmonotonic_boundaries,
        vec!["handler_exit_at_ms<process_exited_at_ms".to_string()]
    );

    let mut incomplete_call = retained_call;
    incomplete_call.output_collected_at_ms = Some(5);
    incomplete_call.output_model_visible_at_ms = None;
    let incomplete_diagnostic = tool_call_lifecycle_diagnostic(&incomplete_call)
        .expect("missing and nonmonotonic retained boundaries must be diagnosed");
    assert_eq!(
        incomplete_diagnostic.missing_boundaries,
        vec!["output_model_visible_at_ms".to_string()]
    );
    assert_eq!(
        incomplete_diagnostic.nonmonotonic_boundaries,
        Vec::<String>::new(),
        "independently rounded collection time must not override exact lifecycle ordering"
    );

    let mut clusters = paired_retained_exec_clusters(100, 50);
    let violations = ab_correctness_violations(&clusters, workload.class(), workload);
    assert!(violations.is_empty(), "{violations:#?}");
    let raw = serde_json::to_value(&clusters[0].b_samples[0])
        .expect("retained lifecycle sample should serialize");
    assert_eq!(raw["retained_write_stdin_poll_count"], 2);
    assert_eq!(
        raw["retained_session_ids"],
        serde_json::json!(["1000", "1000"])
    );
    assert_eq!(raw["retained_process_exit_observed"], true);
    assert_eq!(raw["retained_process_cleanup_complete"], true);
    assert!(raw.get("incomplete_tool_lifecycles").is_none());
    let legacy_sample: Sample = serde_json::from_value(raw)
        .expect("samples without lifecycle diagnostics must remain compatible");
    assert!(legacy_sample.incomplete_tool_lifecycles.is_empty());

    let mut incomplete_sample = valid_retained_exec_sample(50);
    incomplete_sample.incomplete_lifecycle_calls = 1;
    incomplete_sample.incomplete_tool_lifecycles = vec![incomplete_diagnostic];
    incomplete_sample.lifecycle_complete = false;
    incomplete_sample.latency_eligible = false;
    record_retained_lifecycle_coverage_failures(&mut incomplete_sample);
    incomplete_sample.failed = !incomplete_sample.failure_codes.is_empty();
    assert!(incomplete_sample.failed);
    assert_eq!(
        incomplete_sample.failure_codes,
        vec![
            "incomplete_tool_lifecycle".to_string(),
            "lifecycle_coverage".to_string(),
            "latency_ineligible".to_string()
        ]
    );
    let incomplete_raw = serde_json::to_value(&incomplete_sample)
        .expect("incomplete lifecycle diagnostics should serialize");
    assert_eq!(
        incomplete_raw["incomplete_tool_lifecycles"][0]["call_id"],
        "retained-call-0"
    );
    assert_eq!(
        incomplete_raw["incomplete_tool_lifecycles"][0]["missing_boundaries"],
        serde_json::json!(["output_model_visible_at_ms"])
    );
    assert_eq!(
        incomplete_raw["incomplete_tool_lifecycles"][0]["nonmonotonic_boundaries"],
        serde_json::Value::Null
    );
    let mut incomplete_clusters = paired_retained_exec_clusters(100, 50);
    incomplete_clusters[0].b_samples[0] = incomplete_sample;
    let incomplete_violations =
        ab_correctness_violations(&incomplete_clusters, workload.class(), workload);
    assert!(
        incomplete_violations
            .iter()
            .any(|violation| violation.contains("incomplete_tool_lifecycle"))
    );
    assert!(
        incomplete_violations
            .iter()
            .any(|violation| violation.contains("coverage_incomplete"))
    );
    assert!(
        incomplete_violations
            .iter()
            .any(|violation| violation.contains("retained_process_lifecycle"))
    );

    clusters[0].b_samples[0].retained_write_stdin_poll_count = 1;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("retained_process_lifecycle"))
    );
    clusters[0].b_samples[0].retained_write_stdin_poll_count = 2;
    clusters[0].b_samples[0].retained_session_ids[1] = "1001".to_string();
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("retained_process_lifecycle"))
    );
    clusters[0].b_samples[0].retained_session_ids[1] = "1000".to_string();
    clusters[0].b_samples[0].retained_process_cleanup_complete = false;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("retained_process_lifecycle"))
    );
    clusters[0].b_samples[0].retained_process_cleanup_complete = true;
    clusters[0].b_samples[0].tool_call_graph[1].tool_name = "exec_command".to_string();
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("tool_graph_identity"))
    );
    clusters[0].b_samples[0].tool_call_graph[1].tool_name = "write_stdin".to_string();
    clusters[0].b_samples[0].provider_input_tokens += 1;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("B:token_usage"))
    );
}

#[test]
fn ab_overlay_abort_direct_nested_in_flight_is_exact_and_routable() {
    use codex_protocol::protocol::ToolLifecycleBoundary;
    use codex_protocol::protocol::TurnTimingToolLifecycleEvent;

    let workload = AbWorkload::AbortDirectNestedInFlight;
    assert_eq!(AbWorkload::parse(workload.name()).unwrap(), workload);
    assert_eq!(workload.class(), AbWorkloadClass::CorrectnessOnly);
    assert_eq!(workload.expected_logical_generations(), 1);
    assert_eq!(workload.expected_direct_tool_calls(), 1);
    assert_eq!(workload.expected_nested_tool_calls(), 1);
    assert!(workload.latency_metrics().is_empty());
    let shape = workload.report_shape();
    assert_eq!(shape.subturns_per_sample, 1);
    assert_eq!(shape.logical_generations_per_sample, 1);
    assert_eq!(shape.model_requests_per_sample, 1);
    assert_eq!(shape.direct_outer_calls_per_sample, 1);
    assert_eq!(shape.nested_calls_per_sample, 1);
    assert_eq!(shape.nested_calls_by_outer_call, [1]);
    assert_eq!(
        shape.subturn_terminal_outcome,
        "turn_aborted_after_ordered_direct_nested_closure"
    );
    assert!(shape.latency_metrics.is_empty());
    assert_eq!(ab_fixture_hash(workload).len(), 64);
    assert_eq!(ab_workload_schema_hash(workload).len(), 64);

    let command = parse_ab_worker_args_from(strings(&[
        "--code-mode-host",
        "host",
        "--variant",
        "B",
        "--cluster",
        "1",
        "--workload",
        "abort_direct_nested_in_flight",
        "--warmups",
        "1",
        "--samples",
        "1",
    ]))
    .expect("direct+nested abort worker route should parse");
    let BenchmarkCommand::AbWorker(args) = command else {
        panic!("expected A/B worker command");
    };
    assert_eq!(args.workload, workload);

    let lifecycle_event = |boundary, at_ms| TurnTimingToolLifecycleEvent {
        boundary,
        at_ms,
        ..TurnTimingToolLifecycleEvent::default()
    };
    let direct_call = TurnTimingToolCall {
        call_id: "abort-direct".to_string(),
        tool_name: "exec".to_string(),
        source: TurnTimingToolCallSource::Direct,
        accepted_at_ms: Some(1),
        first_poll_at_ms: Some(2),
        parallel_gate_admitted_at_ms: Some(3),
        handler_entry_at_ms: Some(4),
        // Duration projections are independently rounded to milliseconds,
        // so this can precede the exact handler-return event at abort.
        output_collected_at_ms: Some(5),
        handler_exit_at_ms: Some(6),
        delivered_at_ms: Some(7),
        output_model_visible_at_ms: Some(8),
        lifecycle_events: vec![
            lifecycle_event(ToolLifecycleBoundary::RequestCreated, 1),
            lifecycle_event(ToolLifecycleBoundary::Admitted, 3),
            lifecycle_event(ToolLifecycleBoundary::HandlerStart, 4),
            lifecycle_event(ToolLifecycleBoundary::HandlerReturn, 6),
            lifecycle_event(ToolLifecycleBoundary::RelayEnqueue, 7),
            lifecycle_event(ToolLifecycleBoundary::RelayDelivery, 7),
        ],
        ..TurnTimingToolCall::default()
    };
    assert_eq!(
        tool_call_lifecycle_diagnostic(&direct_call),
        None,
        "exact lifecycle events must outrank independently rounded duration projections",
    );

    let mut nested_call = direct_call;
    nested_call.call_id = "abort-nested".to_string();
    nested_call.tool_name = "request_permissions".to_string();
    nested_call.source = TurnTimingToolCallSource::CodeMode;
    nested_call.parent_call_id = Some("abort-direct".to_string());
    nested_call.delivered_at_ms = None;
    nested_call.output_model_visible_at_ms = None;
    nested_call
        .lifecycle_events
        .retain(|event| event.boundary != ToolLifecycleBoundary::RelayDelivery);
    assert_eq!(
        tool_call_lifecycle_diagnostic(&nested_call),
        None,
        "nested delivery is represented by the persisted outer direct projection",
    );

    let mut missing_handler_exit = nested_call.clone();
    missing_handler_exit.handler_exit_at_ms = None;
    assert_eq!(
        tool_call_lifecycle_diagnostic_for_requirement(
            &missing_handler_exit,
            AbToolLifecycleRequirement::TerminalAbort,
        ),
        None,
        "terminal abort timing stops before supervised handler cleanup completes",
    );
    let missing_diagnostic = tool_call_lifecycle_diagnostic_for_requirement(
        &missing_handler_exit,
        AbToolLifecycleRequirement::Full,
    )
    .expect("non-abort timing must reject a missing handler-return boundary");
    assert!(
        missing_diagnostic
            .missing_boundaries
            .contains(&"handler_exit_at_ms".to_string())
    );

    let mut regressed_events = nested_call.clone();
    regressed_events.lifecycle_events = vec![
        lifecycle_event(ToolLifecycleBoundary::RequestCreated, 1),
        lifecycle_event(ToolLifecycleBoundary::Admitted, 3),
        lifecycle_event(ToolLifecycleBoundary::HandlerStart, 4),
        lifecycle_event(ToolLifecycleBoundary::HandlerReturn, 2),
        lifecycle_event(ToolLifecycleBoundary::RelayEnqueue, 7),
    ];
    let regression_diagnostic = tool_call_lifecycle_diagnostic_for_requirement(
        &regressed_events,
        AbToolLifecycleRequirement::TerminalAbort,
    )
    .expect("terminal abort must reject an exact lifecycle-event regression");
    assert!(
        regression_diagnostic
            .nonmonotonic_boundaries
            .iter()
            .any(|boundary| boundary.contains("lifecycle_event:HandlerReturn<HandlerStart"))
    );

    let mut clusters = paired_abort_direct_nested_clusters();
    let violations = ab_correctness_violations(&clusters, workload.class(), workload);
    assert!(violations.is_empty(), "{violations:#?}");
    let verdict = evaluate_ab_workload(&clusters, workload.class(), workload)
        .expect("correctness-only abort workload should evaluate");
    assert!(verdict.passed);
    assert_eq!(verdict.decision, AbSequentialDecision::Passed);
    assert_eq!(verdict.stop_reason, AbStopReason::CorrectnessOnlyComplete);
    assert!(verdict.latency_gates.is_empty());
    assert!(verdict.latency_diagnostics.is_empty());

    let raw = serde_json::to_value(&clusters[0].b_samples[0])
        .expect("abort lifecycle sample should serialize");
    assert_eq!(
        raw["abort_registered_call_ids"],
        serde_json::json!(["abort-direct", "abort-nested"])
    );
    assert_eq!(
        raw["abort_terminal_outcomes_by_registration"],
        serde_json::json!(["failure", "failure"])
    );
    assert_eq!(raw["abort_barrier_call_id"], "abort-nested");
    assert_eq!(raw["abort_model_resumed_call_count"], 0);
    assert_eq!(raw["forged_turn_complete_observed"], false);
    let mut legacy = raw;
    for field in [
        "abort_registered_call_ids",
        "abort_terminal_outcomes_by_registration",
        "abort_barrier_call_id",
        "abort_model_resumed_call_count",
        "forged_turn_complete_observed",
    ] {
        legacy
            .as_object_mut()
            .expect("sample must serialize as an object")
            .remove(field);
    }
    let legacy: Sample = serde_json::from_value(legacy)
        .expect("older raw samples without abort diagnostics must remain readable");
    assert!(legacy.abort_registered_call_ids.is_empty());
    assert!(legacy.abort_terminal_outcomes_by_registration.is_empty());
    assert!(legacy.abort_barrier_call_id.is_none());

    clusters[0].b_samples[0].abort_registered_call_ids.reverse();
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("abort_direct_nested_lifecycle"))
    );
    clusters[0].b_samples[0] = valid_abort_direct_nested_sample();
    clusters[0].b_samples[0]
        .tool_closure
        .as_mut()
        .unwrap()
        .persisted_count = 1;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("tool_closure_mismatch"))
    );
    clusters[0].b_samples[0] = valid_abort_direct_nested_sample();
    clusters[0].b_samples[0].terminal_event = "turn_complete".to_string();
    clusters[0].b_samples[0].forged_turn_complete_observed = true;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("abort_direct_nested_lifecycle"))
    );
    clusters[0].b_samples[0] = valid_abort_direct_nested_sample();
    clusters[0].b_samples[0].latency_eligible = true;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("abort_direct_nested_lifecycle"))
    );
}

#[test]
fn ab_overlay_abort_retained_process_is_exact_and_routable() {
    let workload = AbWorkload::AbortRetainedProcess;
    assert_eq!(AB_ABORT_RETAINED_YIELD_TIME_MS, 10);
    assert_eq!(AbWorkload::parse(workload.name()).unwrap(), workload);
    assert_eq!(workload.class(), AbWorkloadClass::CorrectnessOnly);
    assert_eq!(workload.expected_logical_generations(), 1);
    assert_eq!(workload.expected_direct_tool_calls(), 1);
    assert_eq!(workload.expected_nested_tool_calls(), 0);
    assert!(workload.latency_metrics().is_empty());
    let shape = workload.report_shape();
    assert_eq!(shape.subturns_per_sample, 1);
    assert_eq!(shape.logical_generations_per_sample, 1);
    assert_eq!(shape.model_requests_per_sample, 1);
    assert_eq!(shape.direct_outer_calls_per_sample, 1);
    assert_eq!(shape.nested_calls_per_sample, 0);
    assert_eq!(shape.nested_calls_by_outer_call, [0]);
    assert_eq!(
        shape.subturn_terminal_outcome,
        "turn_aborted_after_exact_result_persistence_and_process_cleanup"
    );
    assert!(shape.latency_metrics.is_empty());
    assert_eq!(ab_fixture_hash(workload).len(), 64);
    assert_eq!(ab_workload_schema_hash(workload).len(), 64);
    let initial_request = serde_json::json!({
        "input": [{"role": "user", "content": AB_ABORT_RETAINED_PROMPT}],
    });
    let continuation_request = serde_json::json!({
        "input": [
            {"role": "user", "content": AB_ABORT_RETAINED_PROMPT},
            {
                "type": "function_call_output",
                "call_id": "retained-call",
                "output": "Process running with session ID 1000",
            },
        ],
    });
    assert_eq!(
        abort_retained_process_response_route(&initial_request),
        AbortRetainedProcessResponseRoute::IssueExecCommand,
        "a warmup's initial request must issue the retained process",
    );
    assert_eq!(
        abort_retained_process_response_route(&continuation_request),
        AbortRetainedProcessResponseRoute::RejectUnexpectedResume,
        "a continuation after the retained result must expose an invalid resume",
    );
    assert_eq!(
        abort_retained_process_response_route(&initial_request),
        AbortRetainedProcessResponseRoute::IssueExecCommand,
        "a rolled-back sample must route from request state, not the monotonic response ID",
    );
    assert_eq!(
        retained_abort_identity_barrier(
            "turn-1",
            "turn-1",
            "call-1".to_string(),
            Some("process-1".to_string()),
        )
        .unwrap(),
        Some(("call-1".to_string(), "process-1".to_string())),
        "ExecCommandBegin identity must release the first abort barrier before READY output",
    );
    assert_eq!(
        retained_abort_identity_barrier(
            "turn-1",
            "turn-other",
            "call-other".to_string(),
            Some("process-other".to_string()),
        )
        .unwrap(),
        None,
        "an exec identity from another turn must not release the barrier",
    );
    assert!(
        retained_abort_identity_barrier("turn-1", "turn-1", "call-1".to_string(), None,).is_err()
    );

    let command = parse_ab_worker_args_from(strings(&[
        "--code-mode-host",
        "host",
        "--variant",
        "B",
        "--cluster",
        "1",
        "--workload",
        "abort_retained_process",
        "--warmups",
        "1",
        "--samples",
        "1",
    ]))
    .expect("retained-process abort worker route should parse");
    let BenchmarkCommand::AbWorker(args) = command else {
        panic!("expected A/B worker command");
    };
    assert_eq!(args.workload, workload);

    let mut clusters = paired_abort_retained_process_clusters();
    let violations = ab_correctness_violations(&clusters, workload.class(), workload);
    assert!(violations.is_empty(), "{violations:#?}");
    let verdict = evaluate_ab_workload(&clusters, workload.class(), workload)
        .expect("correctness-only retained abort workload should evaluate");
    assert!(verdict.passed);
    assert_eq!(verdict.decision, AbSequentialDecision::Passed);
    assert_eq!(verdict.stop_reason, AbStopReason::CorrectnessOnlyComplete);
    assert!(verdict.latency_gates.is_empty());
    assert!(verdict.latency_diagnostics.is_empty());

    let raw = serde_json::to_value(&clusters[0].b_samples[0])
        .expect("retained abort lifecycle sample should serialize");
    assert_eq!(raw["retained_process_owned_before_abort"], true);
    assert_eq!(raw["retained_process_count_before_abort"], 1);
    assert_eq!(raw["retained_abort_process_id"], "process-1000");
    assert_eq!(raw["retained_abort_persisted_result_count"], 1);
    assert_eq!(raw["retained_abort_cancellation_observed"], true);
    assert_eq!(raw["retained_process_cleanup_complete"], true);
    assert_eq!(raw["abort_model_resumed_call_count"], 0);
    assert_eq!(raw["forged_turn_complete_observed"], false);
    let mut legacy = raw;
    for field in [
        "retained_process_owned_before_abort",
        "retained_process_count_before_abort",
        "retained_abort_process_id",
        "retained_abort_persisted_result_count",
        "retained_abort_cancellation_observed",
    ] {
        legacy
            .as_object_mut()
            .expect("sample must serialize as an object")
            .remove(field);
    }
    let legacy: Sample = serde_json::from_value(legacy)
        .expect("older raw samples without retained-abort diagnostics must remain readable");
    assert!(!legacy.retained_process_owned_before_abort);
    assert_eq!(legacy.retained_process_count_before_abort, 0);
    assert!(legacy.retained_abort_process_id.is_none());
    assert_eq!(legacy.retained_abort_persisted_result_count, 0);
    assert!(!legacy.retained_abort_cancellation_observed);

    clusters[0].b_samples[0].retained_process_count_before_abort = 0;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("abort_retained_process_lifecycle"))
    );
    clusters[0].b_samples[0] = valid_abort_retained_process_sample();
    clusters[0].b_samples[0].retained_abort_persisted_result_count = 0;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("abort_retained_process_lifecycle"))
    );
    clusters[0].b_samples[0] = valid_abort_retained_process_sample();
    clusters[0].b_samples[0].retained_process_cleanup_complete = false;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("abort_retained_process_lifecycle"))
    );
    clusters[0].b_samples[0] = valid_abort_retained_process_sample();
    clusters[0].b_samples[0].abort_model_resumed_call_count = 1;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("abort_retained_process_lifecycle"))
    );
    clusters[0].b_samples[0] = valid_abort_retained_process_sample();
    clusters[0].b_samples[0]
        .tool_closure
        .as_mut()
        .unwrap()
        .persisted_count = 0;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("tool_closure_mismatch"))
    );
    clusters[0].b_samples[0] = valid_abort_retained_process_sample();
    clusters[0].b_samples[0].terminal_event = "turn_complete".to_string();
    clusters[0].b_samples[0].forged_turn_complete_observed = true;
    assert!(
        ab_correctness_violations(&clusters, workload.class(), workload)
            .iter()
            .any(|violation| violation.contains("abort_retained_process_lifecycle"))
    );
}

#[tokio::test]
async fn abort_retained_process_cleanup_waits_past_the_terminal_event() {
    let polls = AtomicUsize::new(0);

    assert!(
        wait_for_retained_process_cleanup(|| {
            let complete = polls.fetch_add(1, Ordering::SeqCst) > 0;
            async move { complete }
        })
        .await
    );
    assert_eq!(polls.load(Ordering::SeqCst), 2);
}

#[test]
fn retained_process_cleanup_poll_supersedes_terminal_live_process_snapshot() {
    let mut cleaned = Sample {
        unexpected_live_processes: 1,
        ..Sample::default()
    };
    apply_retained_process_cleanup_observation(&mut cleaned, true);
    assert!(cleaned.retained_process_cleanup_complete);
    assert_eq!(cleaned.unexpected_live_processes, 0);

    let mut still_live = Sample {
        unexpected_live_processes: 1,
        ..Sample::default()
    };
    apply_retained_process_cleanup_observation(&mut still_live, false);
    assert!(!still_live.retained_process_cleanup_complete);
    assert_eq!(still_live.unexpected_live_processes, 1);
}

#[test]
fn correctness_only_workloads_report_advisory_latency_gates() {
    // `code_mode_high_volume` runs identical work on both sides, so a silent
    // latency regression there previously surfaced as `passed` with zero
    // recorded gates. It must now carry measurable gates.
    assert!(
        !AbWorkload::CodeModeHighVolume.latency_metrics().is_empty(),
        "code_mode_high_volume must publish latency metrics"
    );
    assert!(
        AbWorkload::CodeModeHighVolume
            .latency_metrics()
            .iter()
            .any(|metric| metric.name() == "end_to_end"),
        "the workload's total turn cost must be observable"
    );

    // Workloads whose two sides do unequal work stay unmeasured: the baseline
    // omits tool outputs the overlay restores, and the abort workloads keep raw
    // baseline behavior, so a ratio would compare different work.
    for workload in [
        AbWorkload::ParallelSafeTripleDirect,
        AbWorkload::AbortDirectNestedInFlight,
        AbWorkload::AbortRetainedProcess,
    ] {
        assert!(
            workload.latency_metrics().is_empty(),
            "workload `{}` compares unequal work and must stay unmeasured",
            workload.name()
        );
    }

    // Advisory, not hard: these run at one pair per cluster, where a p95 bound
    // is far too wide to enforce.
    let final_config = AbExecutionProfile::Final.config();
    assert!(final_config.latency_hard_gate);
    assert_eq!(
        final_config.latency_gate_mode(AbWorkloadClass::CorrectnessOnly),
        AbLatencyGateMode::Advisory
    );
    assert_eq!(
        final_config.looks_for(AbWorkload::CodeModeHighVolume),
        AB_CORRECTNESS_ONLY_LOOKS,
        "gating must not multiply this workload's sampling cost"
    );
}

#[test]
fn ab_overlay_execution_profiles_are_exact() {
    let quick = AbExecutionProfile::Quick.config();
    assert_eq!(quick.warmups, 1);
    assert_eq!(quick.clusters, 2);
    assert_eq!(quick.looks, [10]);
    assert_eq!(quick.cap, Duration::from_secs(5 * 60));
    assert!(!quick.latency_hard_gate);
    assert_eq!(
        quick.looks_for(AbWorkload::CodeModeHighVolume),
        AB_CORRECTNESS_ONLY_LOOKS
    );
    assert_eq!(
        quick.looks_for(AbWorkload::AbortDirectNestedInFlight),
        AB_CORRECTNESS_ONLY_LOOKS
    );
    assert_eq!(
        quick.looks_for(AbWorkload::LongHistoryNoToolInitial),
        AB_QUICK_LOOKS
    );
    assert_eq!(
        quick.latency_gate_mode(AbWorkloadClass::Latency),
        AbLatencyGateMode::Advisory
    );

    let batch = AbExecutionProfile::Batch.config();
    assert_eq!(batch.warmups, 2);
    assert_eq!(batch.clusters, 2);
    assert_eq!(batch.looks, [20]);
    assert_eq!(batch.cap, Duration::from_secs(10 * 60));
    for profile in [
        AbExecutionProfile::Quick,
        AbExecutionProfile::Batch,
        AbExecutionProfile::Final,
    ] {
        assert_eq!(
            ab_profile_workloads(profile, &[]).unwrap(),
            ab_controller_workloads()
        );
        assert_eq!(
            ab_profile_workloads(profile, &[AbWorkload::StableContextWarmCache]).unwrap(),
            [AbWorkload::StableContextWarmCache]
        );
    }

    let final_config = AbExecutionProfile::Final.config();
    assert_eq!(final_config.warmups, 3);
    assert_eq!(final_config.clusters, 14);
    assert_eq!(final_config.looks, [10]);
    assert_eq!(final_config.cap, Duration::from_secs(30 * 60));
    assert!(final_config.latency_hard_gate);
    assert_eq!(
        final_config.latency_gate_mode(AbWorkloadClass::Latency),
        AbLatencyGateMode::Hard
    );
    assert_eq!(
        final_config.latency_gate_mode(AbWorkloadClass::CorrectnessOnly),
        AbLatencyGateMode::Advisory
    );
    assert!((final_config.ucb_quantile() - (1.0 - AB_FAMILY_WISE_ALPHA)).abs() < f64::EPSILON);
    assert_eq!(
        ab_profile_workloads(AbExecutionProfile::Final, &[]).unwrap(),
        ab_controller_workloads()
    );

    let replay = AbExecutionProfile::Replay.config();
    assert_eq!(replay.warmups, 0);
    assert_eq!(replay.clusters, 1);
    assert_eq!(replay.looks, AB_REPLAY_LOOKS);
    assert_eq!(replay.max_pairs_per_cluster(), AB_REPLAY_PAIRS);
    assert_eq!(replay.cap, Duration::from_secs(10 * 60));
    assert!(replay.latency_hard_gate);
    assert_eq!(
        replay.latency_gate_mode(AbWorkloadClass::Latency),
        AbLatencyGateMode::Hard
    );
    assert_eq!(
        ab_profile_workloads(AbExecutionProfile::Replay, &[]).unwrap(),
        [AbWorkload::SessionReplay]
    );
    assert!(
        ab_profile_workloads(
            AbExecutionProfile::Replay,
            &[AbWorkload::LongHistoryNoToolInitial]
        )
        .is_err()
    );
    assert_ne!(
        ab_profile_configuration_hash(quick, &[AbWorkload::LongHistoryNoToolInitial]),
        ab_profile_configuration_hash(
            batch,
            &[
                AbWorkload::LongHistoryNoToolInitial,
                AbWorkload::CodeModeHighVolume,
            ]
        )
    );

    let mut advisory_clusters = paired_request_cache_clusters_with_pairs(
        AbWorkload::LongHistoryNoToolInitial,
        100,
        200,
        quick.max_pairs_per_cluster(),
    );
    advisory_clusters.truncate(quick.clusters);
    for cluster in &mut advisory_clusters {
        cluster.a_first.truncate(quick.max_pairs_per_cluster());
        cluster.a_samples.truncate(quick.max_pairs_per_cluster());
        cluster.b_samples.truncate(quick.max_pairs_per_cluster());
    }
    let advisory = evaluate_ab_workload_with_config(
        &advisory_clusters,
        AbWorkloadClass::Latency,
        AbWorkload::LongHistoryNoToolInitial,
        quick,
        quick.max_pairs_per_cluster(),
    )
    .expect("quick advisory gate should be measurable");
    assert_eq!(advisory.decision, AbSequentialDecision::Passed);
    assert_eq!(advisory.stop_reason, AbStopReason::AdvisoryComplete);
    assert!(advisory.passed);
    assert!(advisory.latency_gates.iter().any(|gate| !gate.passed));

    let mut hard_failure =
        paired_request_cache_clusters(AbWorkload::LongHistoryNoToolInitial, 100, 200);
    for cluster in &mut hard_failure {
        cluster.a_first.truncate(AB_FINAL_LOOKS[0]);
        cluster.a_samples.truncate(AB_FINAL_LOOKS[0]);
        cluster.b_samples.truncate(AB_FINAL_LOOKS[0]);
    }
    let mut correctness_failure = hard_failure.clone();
    correctness_failure[0].b_samples[0].failed = true;
    correctness_failure[0].b_samples[0]
        .failure_codes
        .push("intentional_pointwise_failure".to_string());
    let correctness_failure = evaluate_ab_workload_with_config(
        &correctness_failure,
        AbWorkloadClass::Latency,
        AbWorkload::LongHistoryNoToolInitial,
        final_config,
        AB_FINAL_LOOKS[0],
    )
    .expect("pointwise correctness failure should not enter the bootstrap");
    assert_eq!(correctness_failure.decision, AbSequentialDecision::Failed);
    assert_eq!(
        correctness_failure.stop_reason,
        AbStopReason::CorrectnessFailure
    );
    assert!(correctness_failure.latency_gates.is_empty());

    let hard_failure = evaluate_ab_workload_with_config(
        &hard_failure,
        AbWorkloadClass::Latency,
        AbWorkload::LongHistoryNoToolInitial,
        final_config,
        AB_FINAL_LOOKS[0],
    )
    .expect("final hard gate should be measurable");
    assert_eq!(hard_failure.decision, AbSequentialDecision::Failed);
    assert_eq!(hard_failure.stop_reason, AbStopReason::LatencyClearFailure);
    assert!(!hard_failure.passed);
}

#[test]
fn ab_overlay_session_replay_enforces_every_pair_without_bootstrap() {
    let config = AbExecutionProfile::Replay.config();
    assert_eq!(
        AbWorkload::SessionReplay
            .latency_metrics()
            .iter()
            .map(|metric| metric.name())
            .collect::<Vec<_>>(),
        vec![
            "end_to_end",
            "controllable_turn",
            "request_preparation",
            "sampling_to_call",
            "post_tool_handoff",
            "parallel_gate_wait",
            "projection_persistence",
            "terminalization",
        ]
    );
    let clusters = vec![valid_session_replay_cluster()];
    let verdict = evaluate_ab_workload_with_config(
        &clusters,
        AbWorkloadClass::Latency,
        AbWorkload::SessionReplay,
        config,
        AB_REPLAY_PAIRS,
    )
    .expect("complete ten-pair replay should be measurable");
    assert!(verdict.passed, "{:?}", verdict.correctness_violations);
    assert_eq!(verdict.decision, AbSequentialDecision::Passed);
    assert_eq!(verdict.latency_gates.len(), AbLatencyMetric::REPLAY.len());
    assert!(verdict.latency_diagnostics.is_empty());
    assert!(verdict.latency_gates.iter().all(|gate| {
        gate.pairs_per_cluster == AB_REPLAY_PAIRS
            && gate.point_median_ratio == 0.5
            && gate.point_p95_ratio == 0.5
            && gate.target_ratio == 0.75
            && gate.median_ratio_ucb_limit == 0.75
            && gate.p95_ratio_ucb_limit == 0.75
            && gate.lcb_quantile == 0.0
            && gate.ucb_quantile == 1.0
            && gate.median_ratio_lcb == gate.point_median_ratio
            && gate.median_ratio_ucb == gate.point_median_ratio
            && gate.p95_ratio_lcb == gate.point_p95_ratio
            && gate.p95_ratio_ucb == gate.point_p95_ratio
            && gate.passed
    }));

    let worker = parse_ab_worker_args_from(strings(&[
        "--code-mode-host",
        "host",
        "--variant",
        "B",
        "--cluster",
        "1",
        "--workload",
        "session_replay",
        "--warmups",
        "0",
        "--samples",
        "10",
    ]))
    .expect("replay worker must accept zero warmups and ten samples");
    let BenchmarkCommand::AbWorker(worker) = worker else {
        panic!("expected replay worker command");
    };
    assert_eq!(worker.workload, AbWorkload::SessionReplay);
    assert_eq!(worker.warmups, 0);
    assert_eq!(worker.samples, AB_REPLAY_PAIRS);
}

#[test]
fn ab_overlay_session_replay_requires_25_percent_improvement() {
    assert_eq!(AB_REPLAY_REQUIRED_IMPROVEMENT_PERCENT, 25);
    assert_eq!(AB_REPLAY_RATIO_TARGET, 0.75);
    assert!(replay_latency_pair_passes(100, 75));
    assert!(!replay_latency_pair_passes(100, 76));
    assert_eq!(replay_latency_limit_ns(101), 75);

    let mut clusters = vec![valid_session_replay_cluster()];
    clusters[0].b_samples[4].preparation_ns = 76;
    let verdict = evaluate_ab_workload_with_config(
        &clusters,
        AbWorkloadClass::Latency,
        AbWorkload::SessionReplay,
        AbExecutionProfile::Replay.config(),
        AB_REPLAY_PAIRS,
    )
    .expect("a replay pair above the 25%-faster limit should produce a verdict");
    assert!(!verdict.passed);
    assert!(verdict.correctness_violations.iter().any(|violation| {
        violation.contains("pair:4:request_preparation:ratio")
            && violation.contains("25pct_faster_limit=75")
    }));
}

#[test]
fn ab_overlay_session_replay_rejects_complete_turn_regression_when_local_lanes_pass() {
    let config = AbExecutionProfile::Replay.config();
    let mut clusters = vec![valid_session_replay_cluster()];
    for sample in &mut clusters[0].b_samples {
        sample.duration_ns = 76;
    }

    let verdict = evaluate_ab_workload_with_config(
        &clusters,
        AbWorkloadClass::Latency,
        AbWorkload::SessionReplay,
        config,
        AB_REPLAY_PAIRS,
    )
    .expect("complete-turn regression must produce a retained verdict");

    assert!(!verdict.passed);
    assert!(
        verdict
            .correctness_violations
            .iter()
            .any(|violation| violation.contains("end_to_end:ratio")),
        "the replay gate must reject total-turn latency even when every local timing lane passes: {:#?}",
        verdict.correctness_violations
    );
}

#[test]
fn ab_overlay_session_replay_requires_retained_cleanup_and_no_avoidable_resume() {
    let mut sample = valid_session_replay_sample(true, 50);
    let raw = serde_json::to_value(&sample).expect("replay sample should serialize");
    assert_eq!(raw["retained_write_stdin_poll_count"], 2);
    assert_eq!(raw["abort_model_resumed_call_count"], 0);
    assert_eq!(raw["retained_process_cleanup_complete"], true);

    let mut violations = Vec::new();
    replay_sample_contract_violations(1, 0, "B", &sample, &mut violations);
    assert!(violations.is_empty(), "{violations:#?}");

    sample.retained_process_cleanup_complete = false;
    replay_sample_contract_violations(1, 0, "B", &sample, &mut violations);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("retained_abort_contract"))
    );

    sample.retained_process_cleanup_complete = true;
    sample.abort_model_resumed_call_count = 1;
    violations.clear();
    replay_sample_contract_violations(1, 0, "B", &sample, &mut violations);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("retained_abort_contract"))
    );
}

#[test]
fn tool_result_correctness_replay_requires_a_terminal_defect_and_b_artifact_recovery() {
    let mut a = valid_session_replay_sample(false, 100);
    let mut violations = Vec::new();
    replay_sample_contract_violations(1, 0, "A", &a, &mut violations);
    assert!(violations.is_empty(), "{violations:#?}");
    assert_eq!(a.failure_terminalized_subturns, 1);
    assert_eq!(a.replay_subturns[1].terminal_event, "error");
    assert!(!a.replay_subturns[1].follow_up_artifact_present);

    a.replay_subturns[1].follow_up_artifact_present = true;
    violations.clear();
    replay_sample_contract_violations(1, 0, "A", &a, &mut violations);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("recoverable_exec_failure"))
    );

    let mut b = valid_session_replay_sample(true, 50);
    b.replay_subturns[1].follow_up_artifact_present = false;
    violations.clear();
    replay_sample_contract_violations(1, 0, "B", &b, &mut violations);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("recoverable_exec_failure"))
    );
}

#[test]
fn ab_overlay_session_replay_rejects_incomplete_or_over_limit_pairs() {
    let config = AbExecutionProfile::Replay.config();
    let mut clusters = vec![valid_session_replay_cluster()];
    clusters[0].b_samples[4].preparation_ns = 76;
    let verdict = evaluate_ab_workload_with_config(
        &clusters,
        AbWorkloadClass::Latency,
        AbWorkload::SessionReplay,
        config,
        AB_REPLAY_PAIRS,
    )
    .expect("failed replay pair must produce a retained verdict");
    assert!(!verdict.passed);
    assert!(
        verdict
            .correctness_violations
            .iter()
            .any(|violation| violation.contains("pair:4:request_preparation:ratio"))
    );

    clusters[0].b_samples.pop();
    clusters[0].a_samples.pop();
    clusters[0].a_first.pop();
    let incomplete = evaluate_ab_workload_with_config(
        &clusters,
        AbWorkloadClass::Latency,
        AbWorkload::SessionReplay,
        config,
        AB_REPLAY_PAIRS,
    )
    .expect("incomplete replay must produce a failed retained verdict");
    assert!(!incomplete.passed);
    assert!(incomplete.latency_gates.is_empty());
    assert!(
        incomplete
            .correctness_violations
            .iter()
            .any(|violation| violation.contains("requires_exactly_10_complete_pairs"))
    );
}

#[test]
fn ab_overlay_session_replay_stages_ignore_projected_output_count() {
    let action_stage = AtomicUsize::new(0);
    let failure_stage = AtomicUsize::new(0);
    let baseline_routes =
        [0, 3, 3, 4, 3, 1, 1, 1, 1, 0].map(|output_count| ReplayActionResponseRoute::Action {
            action_first: false,
            output_count,
        });
    let baseline_stages =
        baseline_routes.map(|route| replay_response_stage(route, &action_stage, &failure_stage));
    assert_eq!(baseline_stages, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);

    reset_replay_response_stages(&action_stage, &failure_stage);
    let candidate_routes = [0, 3, 1, 1].map(|output_count| ReplayActionResponseRoute::Action {
        action_first: true,
        output_count,
    });
    let candidate_stages =
        candidate_routes.map(|route| replay_response_stage(route, &action_stage, &failure_stage));
    assert_eq!(candidate_stages, [0, 1, 2, 3]);

    let failure_routes = [0, 1, 1, 1].map(|output_count| ReplayActionResponseRoute::Failure {
        action_first: false,
        output_count,
    });
    let failure_stages =
        failure_routes.map(|route| replay_response_stage(route, &action_stage, &failure_stage));
    assert_eq!(failure_stages, [0, 1, 2, 3]);

    reset_replay_response_stages(&action_stage, &failure_stage);
    let candidate_failure_routes =
        [0, 1, 1].map(|output_count| ReplayActionResponseRoute::Failure {
            action_first: true,
            output_count,
        });
    let candidate_failure_stages = candidate_failure_routes
        .map(|route| replay_response_stage(route, &action_stage, &failure_stage));
    assert_eq!(candidate_failure_stages, [0, 1, 2]);
}

#[test]
fn ab_overlay_session_replay_routes_custom_tool_output_to_follow_up() {
    assert_eq!(
        AB_REPLAY_TEST_PATH,
        "codex-rs/core/benches/turn_latency/tests.rs"
    );
    assert!(
        controller_repository_root()
            .join(AB_REPLAY_TEST_PATH)
            .is_file(),
        "session replay must target the benchmark's registered direct test"
    );
    assert_eq!(
        AB_REPLAY_ACTION_CONTENTION_SOURCE
            .matches("tools.update_plan(plan)")
            .count(),
        5
    );
    assert_eq!(
        AB_REPLAY_ACTION_CONTENTION_SOURCE
            .matches("status: \"in_progress\"")
            .count(),
        1
    );
    assert!(!AB_REPLAY_ACTION_CONTENTION_SOURCE.contains("status: \"completed\""));
    assert!(AB_REPLAY_ACTION_CONTENTION_SOURCE.contains(AB_REPLAY_VALIDATION_SELECTOR));

    let validation_events = replay_direct_validation_events("validation-response", 2_304);
    let validation_arguments: serde_json::Value = serde_json::from_str(
        validation_events[1]["item"]["arguments"]
            .as_str()
            .expect("validation call must carry serialized arguments"),
    )
    .expect("validation call arguments must be valid JSON");
    assert_eq!(validation_arguments["program"], "python");
    assert_eq!(
        validation_arguments["args"],
        serde_json::json!(["-m", "unittest", AB_REPLAY_VALIDATION_SELECTOR])
    );
    assert_eq!(
        validation_arguments["validation"]["covered_paths"],
        serde_json::json!(AB_REPLAY_SOURCE_PATHS)
    );
    assert_eq!(
        validation_arguments["validation"]
            .as_object()
            .map(serde_json::Map::len),
        Some(1)
    );

    let artifact_events = replay_code_mode_exec_command_events(
        "artifact-response",
        "artifact-repair",
        Path::new("benchmark-worker"),
        Path::new("benchmark-workspace"),
        "artifact",
        &[AB_REPLAY_FOLLOW_UP_ARTIFACT_PATH],
        1_792,
    );
    let artifact_source = artifact_events[1]["item"]["input"]
        .as_str()
        .expect("artifact repair must be a Code Mode exec source");
    assert!(artifact_source.contains("tools.exec_command"));
    assert!(artifact_source.contains(AB_REPLAY_FOLLOW_UP_ARTIFACT_PATH));
    assert!(artifact_source.contains("ab-replay-command"));

    let initial = serde_json::json!({
        "input": [{
            "role": "user",
            "content": AB_REPLAY_ACTION_PROMPT,
        }],
    });
    assert_eq!(
        replay_action_response_route(&initial, true),
        ReplayActionResponseRoute::Action {
            action_first: true,
            output_count: 0,
        }
    );

    let continuation = serde_json::json!({
        "input": [
            {
                "role": "user",
                "content": AB_REPLAY_ACTION_PROMPT,
            },
            {
                "type": "custom_tool_call",
                "call_id": "replay-exec",
                "name": "exec",
                "input": "text('done')",
            },
            {
                "type": "custom_tool_call_output",
                "call_id": "replay-exec",
                "output": "done",
            },
        ],
    });
    assert_eq!(
        replay_action_response_route(&continuation, true),
        ReplayActionResponseRoute::Action {
            action_first: true,
            output_count: 1,
        }
    );

    let sample = valid_session_replay_sample(true, 50);
    assert_eq!(sample.replay_subturns[0].logical_generations, 4);
    assert_eq!(sample.replay_subturns[1].logical_generations, 3);
    let mut violations = Vec::new();
    replay_sample_contract_violations(1, 0, "B", &sample, &mut violations);
    assert!(violations.is_empty(), "{violations:#?}");
}

#[test]
fn ab_overlay_sequential_looks_reuse_cluster_prefixes() {
    let clusters = paired_clusters(100, 50);
    let first_pairs = AB_ITERATIONS / 2;
    let second_pairs = AB_ITERATIONS;
    let first = ab_cluster_prefixes(&clusters, first_pairs).unwrap();
    let second = ab_cluster_prefixes(&clusters, second_pairs).unwrap();
    for (first, second) in first.iter().zip(&second) {
        assert_eq!(first.a_first, second.a_first[..first_pairs]);
        assert_eq!(first.a_samples.len(), first_pairs);
        assert_eq!(first.b_samples.len(), first_pairs);
        assert_eq!(
            serde_json::to_vec(&first.a_samples).unwrap(),
            serde_json::to_vec(&second.a_samples[..first_pairs]).unwrap(),
        );
        assert_eq!(
            serde_json::to_vec(&first.b_samples).unwrap(),
            serde_json::to_vec(&second.b_samples[..first_pairs]).unwrap(),
        );
    }
}

#[test]
fn ab_overlay_cargo_target_dir_is_command_safe() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = fs::canonicalize(temp.path()).unwrap();
    let command_path = cargo_target_dir_for_command(&canonical);
    assert!(command_path.is_absolute());
    assert_eq!(
        dunce::canonicalize(&command_path).unwrap(),
        dunce::canonicalize(&canonical).unwrap(),
        "command-safe target path must retain the same directory identity"
    );
    assert!(
        !command_path.to_string_lossy().starts_with(r"\\?\"),
        "Cargo target paths passed to MSVC must not use the verbatim prefix"
    );
}

#[test]
fn ab_overlay_prepare_target_caches_are_paired_and_isolated() {
    let paired = parse_command_from(strings(&[
        "ab-prepare",
        "--state",
        "state.json",
        "--candidate-repo",
        "repo",
        "--work-root",
        "prepared-builds",
        "--manifest",
        "prepared.json",
        "--baseline-target-dir",
        "baseline-cache",
        "--candidate-target-dir",
        "candidate-cache",
        "--reuse-work-root",
    ]))
    .expect("paired target cache options should parse");
    let BenchmarkCommand::AbPrepare(paired) = paired else {
        panic!("expected A/B prepare command");
    };
    assert_eq!(
        paired.baseline_target_dir,
        Some(PathBuf::from("baseline-cache"))
    );
    assert_eq!(
        paired.candidate_target_dir,
        Some(PathBuf::from("candidate-cache"))
    );
    assert!(paired.reuse_work_root);

    let unpaired_prefix = [
        "ab-prepare",
        "--state",
        "state.json",
        "--candidate-repo",
        "repo",
        "--work-root",
        "prepared-builds",
        "--manifest",
        "prepared.json",
    ];
    for one_sided in [
        ["--baseline-target-dir", "baseline-cache"],
        ["--candidate-target-dir", "candidate-cache"],
    ] {
        assert!(
            parse_command_from(strings(
                &unpaired_prefix
                    .into_iter()
                    .chain(one_sided)
                    .collect::<Vec<_>>(),
            ))
            .is_err(),
            "target cache options must be supplied as an A/B pair"
        );
    }

    let temp = tempfile::tempdir().unwrap();
    let path = |name: &str| {
        let path = temp.path().join(name);
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
    };
    let baseline_repository = path("baseline-repository");
    let candidate_repository = path("candidate-repository");
    let baseline_worktree = path("baseline-worktree");
    let candidate_worktree = path("candidate-worktree");
    let baseline_target = path("baseline-target");
    let candidate_target = path("candidate-target");
    validate_ab_prepare_target_layout(
        &baseline_target,
        &candidate_target,
        &baseline_repository,
        &candidate_repository,
        &baseline_worktree,
        &candidate_worktree,
    )
    .expect("canonical sibling target caches should be isolated");
    assert!(
        validate_ab_prepare_target_layout(
            &baseline_target,
            &baseline_target,
            &baseline_repository,
            &candidate_repository,
            &baseline_worktree,
            &candidate_worktree,
        )
        .is_err(),
        "A/B target caches must be distinct"
    );
    let nested_target = baseline_target.join("nested");
    fs::create_dir_all(&nested_target).unwrap();
    let nested_target = fs::canonicalize(nested_target).unwrap();
    assert!(
        validate_ab_prepare_target_layout(
            &baseline_target,
            &nested_target,
            &baseline_repository,
            &candidate_repository,
            &baseline_worktree,
            &candidate_worktree,
        )
        .is_err(),
        "A/B target caches must not be nested"
    );
    let repository_target = baseline_repository.join("target-cache");
    fs::create_dir_all(&repository_target).unwrap();
    let repository_target = fs::canonicalize(repository_target).unwrap();
    assert!(
        validate_ab_prepare_target_layout(
            &repository_target,
            &candidate_target,
            &baseline_repository,
            &candidate_repository,
            &baseline_worktree,
            &candidate_worktree,
        )
        .is_err(),
        "target caches must be disjoint from source repositories"
    );
}

#[test]
fn ab_overlay_prepare_can_reuse_clean_isolated_worktrees() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("codex-rs").join("core").join("benches")).unwrap();
    git_text(temp.path(), &["init", repo.to_str().unwrap()]).unwrap();
    git_text(
        &repo,
        &["config", "user.email", "benchmark@example.invalid"],
    )
    .unwrap();
    git_text(&repo, &["config", "user.name", "Benchmark Test"]).unwrap();
    fs::write(
        repo.join("codex-rs")
            .join("core")
            .join("benches")
            .join("turn_latency.rs"),
        b"first overlay",
    )
    .unwrap();
    git_text(&repo, &["add", "."]).unwrap();
    git_text(&repo, &["commit", "-m", "first"]).unwrap();
    let first = git_text(&repo, &["rev-parse", "HEAD"]).unwrap();
    fs::write(repo.join("runtime.txt"), b"candidate runtime").unwrap();
    git_text(&repo, &["add", "runtime.txt"]).unwrap();
    git_text(&repo, &["commit", "-m", "candidate"]).unwrap();
    let candidate = git_text(&repo, &["rev-parse", "HEAD"]).unwrap();

    let prepared = temp.path().join("prepared");
    add_detached_worktree(&repo, &first, &prepared).unwrap();
    fs::write(
        prepared
            .join("codex-rs")
            .join("core")
            .join("benches")
            .join("turn_latency.rs"),
        b"benchmark-only overlay",
    )
    .unwrap();
    let untracked_overlay_module = prepared
        .join("codex-rs")
        .join("core")
        .join("benches")
        .join("turn_latency")
        .join("ab_runner.rs");
    fs::create_dir_all(untracked_overlay_module.parent().unwrap()).unwrap();
    fs::write(&untracked_overlay_module, b"benchmark-only module").unwrap();
    reuse_detached_worktree(&repo, &candidate, &prepared).unwrap();

    assert_eq!(
        git_text(&prepared, &["rev-parse", "HEAD"]).unwrap(),
        candidate
    );
    assert_eq!(
        git_text(
            &prepared,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )
        .unwrap(),
        ""
    );
    assert!(!untracked_overlay_module.exists());
}

#[test]
fn ab_overlay_cargo_json_selects_current_turn_latency_artifact() {
    let temp = tempfile::tempdir().unwrap();
    let stale = temp.path().join(executable_name("turn_latency-stale"));
    let current = temp.path().join(executable_name("turn_latency-current"));
    fs::write(&stale, b"stale").unwrap();
    fs::write(&current, b"current").unwrap();
    let messages = [
        serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "name": "turn_latency", "kind": ["bin"] },
            "executable": stale,
        }),
        serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "name": "another_benchmark", "kind": ["bench"] },
            "executable": stale,
        }),
        serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "name": "turn_latency", "kind": ["bench"] },
            "executable": current,
        }),
        serde_json::json!({ "reason": "build-finished", "success": true }),
    ];
    let output = messages
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        select_turn_latency_executable_from_cargo_json(output.as_bytes()).unwrap(),
        current,
        "the current compiler artifact must win over stale directory contents"
    );

    let duplicate = format!("{output}\n{}", messages[2]);
    assert!(
        select_turn_latency_executable_from_cargo_json(duplicate.as_bytes()).is_err(),
        "ambiguous current bench artifacts must be rejected"
    );
    let missing_executable = serde_json::json!({
        "reason": "compiler-artifact",
        "target": { "name": "turn_latency", "kind": ["bench"] },
        "executable": null,
    })
    .to_string();
    assert!(
        select_turn_latency_executable_from_cargo_json(missing_executable.as_bytes()).is_err(),
        "the selected current bench artifact must carry its executable"
    );
}

#[test]
fn ab_overlay_prepared_manifest_is_verified_and_compare_cannot_build() {
    let temp = tempfile::tempdir().unwrap();
    let (baseline_commit, baseline_filtered_tree, baseline) =
        test_prepared_build(temp.path(), "baseline", "baseline");
    let (candidate_commit, candidate_filtered_tree, candidate) =
        test_prepared_build(temp.path(), "candidate", "candidate");
    let (rustc_version, rust_target) = rust_provenance().unwrap();
    let mut manifest = AbPreparedManifest {
        schema_version: AB_PREPARED_MANIFEST_SCHEMA_VERSION,
        baseline_commit,
        candidate_commit,
        baseline_filtered_tree,
        candidate_filtered_tree,
        overlay_sha256: ab_overlay_sha256_at_repository(&controller_repository_root()).unwrap(),
        fixture_matrix_sha256: ab_matrix_hash(ab_all_workloads(), ab_fixture_hash),
        workload_schema_matrix_sha256: ab_matrix_hash(ab_all_workloads(), ab_workload_schema_hash),
        build_configuration_sha256: ab_build_configuration_hash(&rustc_version, &rust_target),
        rustc_version,
        rust_target,
        baseline,
        candidate,
        manifest_payload_sha256: String::new(),
    };
    manifest.manifest_payload_sha256 = prepared_manifest_payload_hash(&manifest).unwrap();
    validate_ab_prepared_manifest(&manifest).unwrap();

    let baseline_overlay = manifest
        .baseline
        .worktree
        .join("codex-rs")
        .join("core")
        .join("benches")
        .join("turn_latency.rs");
    let original_overlay = fs::read(&baseline_overlay).unwrap();
    let mut modified_overlay = original_overlay.clone();
    modified_overlay.extend_from_slice(b"\n");
    fs::write(&baseline_overlay, &modified_overlay).unwrap();
    validate_prepared_worktree(
        &manifest.baseline,
        &manifest.baseline_commit,
        &manifest.baseline_filtered_tree,
        &ab_overlay_sha256_at_repository(&manifest.baseline.worktree).unwrap(),
    )
    .expect("an unstaged benchmark overlay must be the only accepted worktree change");
    fs::write(&baseline_overlay, original_overlay).unwrap();

    let manifest_path = temp.path().join("prepared.json");
    write_new_ab_prepared_manifest(&manifest_path, &manifest).unwrap();
    assert!(
        write_new_ab_prepared_manifest(&manifest_path, &manifest).is_err(),
        "prepare must never replace an existing manifest"
    );
    let builds_before = AB_BUILD_COMMAND_INVOCATIONS.load(Ordering::SeqCst);
    let (resolved, baseline_build, candidate_build) =
        resolve_ab_compare_inputs(&manifest_path).unwrap();
    assert_eq!(
        resolved.manifest_payload_sha256,
        manifest.manifest_payload_sha256
    );
    assert_eq!(baseline_build.worker, manifest.baseline.worker);
    assert_eq!(candidate_build.worker, manifest.candidate.worker);
    assert_eq!(
        AB_BUILD_COMMAND_INVOCATIONS.load(Ordering::SeqCst),
        builds_before,
        "resolving compare inputs must never invoke Cargo"
    );

    let mut aliased = manifest.clone();
    aliased.candidate.worker = aliased.baseline.worker.clone();
    aliased.candidate.worker_sha256 = aliased.baseline.worker_sha256.clone();
    aliased.manifest_payload_sha256 = prepared_manifest_payload_hash(&aliased).unwrap();
    assert!(
        validate_ab_prepared_manifest(&aliased).is_err(),
        "A/B builds must not alias artifacts"
    );

    let original_worker = fs::read(&manifest.candidate.worker).unwrap();
    fs::write(&manifest.candidate.worker, b"tampered-worker").unwrap();
    assert!(resolve_ab_compare_inputs(&manifest_path).is_err());
    fs::write(&manifest.candidate.worker, original_worker).unwrap();
    let unexpected = manifest.baseline.worktree.join("unexpected.txt");
    fs::write(&unexpected, b"unexpected").unwrap();
    assert!(resolve_ab_compare_inputs(&manifest_path).is_err());
    fs::remove_file(unexpected).unwrap();

    let mut with_unknown_field = serde_json::to_value(&manifest).unwrap();
    with_unknown_field.as_object_mut().unwrap().insert(
        "unmodeledConfiguration".to_string(),
        serde_json::json!(true),
    );
    assert!(
        serde_json::from_value::<AbPreparedManifest>(with_unknown_field).is_err(),
        "versioned manifests must reject unknown configuration"
    );

    let mut stale = manifest.clone();
    stale.overlay_sha256 = sha256_bytes(b"different overlay");
    stale.manifest_payload_sha256 = prepared_manifest_payload_hash(&stale).unwrap();
    assert!(validate_ab_prepared_manifest_contract(&stale).is_err());
    let mut corrupt = manifest;
    corrupt.manifest_payload_sha256 = sha256_bytes(b"corrupt payload");
    assert!(validate_ab_prepared_manifest_contract(&corrupt).is_err());

    let prepared = parse_command_from(strings(&[
        "ab-prepare",
        "--state",
        "state.json",
        "--candidate-repo",
        "repo",
        "--work-root",
        "prepared-builds",
        "--manifest",
        "prepared.json",
    ]))
    .unwrap();
    assert!(matches!(prepared, BenchmarkCommand::AbPrepare(_)));
    let compare = parse_command_from(strings(&[
        "ab-compare",
        "--manifest",
        "prepared.json",
        "--report",
        "report.json",
        "--profile",
        "quick",
        "--workload",
        "long_history_no_tool_initial",
    ]))
    .unwrap();
    let BenchmarkCommand::AbCompare(compare) = compare else {
        panic!("expected compare command");
    };
    assert_eq!(compare.manifest, PathBuf::from("prepared.json"));
    assert!(
        parse_command_from(strings(&[
            "ab-compare",
            "--state",
            "state.json",
            "--work-root",
            "new-builds",
            "--report",
            "report.json",
            "--profile",
            "quick",
            "--workload",
            "long_history_no_tool_initial",
        ]))
        .is_err(),
        "ab-compare must not accept inputs that can create or rebuild A/B"
    );
}

#[test]
fn ab_overlay_profile_cli_keeps_workload_selection_independent() {
    let base = ["--manifest", "prepared.json", "--report", "report.json"];
    let quick = parse_ab_compare_args_from(strings(
        &base
            .into_iter()
            .chain([
                "--profile",
                "quick",
                "--workload",
                "stable_context_warm_cache",
            ])
            .collect::<Vec<_>>(),
    ))
    .expect("quick profile with an affected workload should parse");
    let BenchmarkCommand::AbCompare(quick) = quick else {
        panic!("expected A/B compare command");
    };
    assert_eq!(quick.profile, AbExecutionProfile::Quick);
    assert_eq!(
        quick.requested_workloads,
        [AbWorkload::StableContextWarmCache]
    );

    for profile in ["quick", "batch", "final"] {
        let arguments = base
            .into_iter()
            .chain(["--profile", profile])
            .collect::<Vec<_>>();
        let command = parse_ab_compare_args_from(strings(&arguments)).unwrap_or_else(|error| {
            panic!("{profile} full-matrix selection should parse: {error}")
        });
        let BenchmarkCommand::AbCompare(arguments) = command else {
            panic!("expected A/B compare command");
        };
        assert!(arguments.requested_workloads.is_empty());

        let arguments = base
            .into_iter()
            .chain([
                "--profile",
                profile,
                "--workload",
                "stable_context_warm_cache",
            ])
            .collect::<Vec<_>>();
        let command = parse_ab_compare_args_from(strings(&arguments)).unwrap_or_else(|error| {
            panic!("{profile} targeted workload selection should parse: {error}")
        });
        let BenchmarkCommand::AbCompare(arguments) = command else {
            panic!("expected A/B compare command");
        };
        assert_eq!(
            arguments.requested_workloads,
            [AbWorkload::StableContextWarmCache]
        );
    }

    for invalid in [
        vec!["--profile", "quick", "--workload", ""],
        vec![
            "--profile",
            "quick",
            "--workload",
            "stable_context_warm_cache",
            "--workload",
            "stable_context_warm_cache",
        ],
        vec!["--profile", "quick", "--workload", "session_replay"],
        vec!["--profile", "batch", "--workload", "session_replay"],
        vec!["--profile", "final", "--workload", "session_replay"],
    ] {
        let arguments = base.into_iter().chain(invalid).collect::<Vec<_>>();
        assert!(
            parse_ab_compare_args_from(strings(&arguments)).is_err(),
            "invalid profile arguments unexpectedly parsed: {arguments:?}"
        );
    }
}

#[test]
fn ab_overlay_cli_rejects_duplicate_identity_flags() {
    let cases = [
        (
            "--repo",
            &["ab-capture", "--repo", "first", "--repo", "second"][..],
        ),
        (
            "--state",
            &["ab-capture", "--state", "first", "--state", "second"][..],
        ),
        (
            "--state",
            &["ab-prepare", "--state", "first", "--state", "second"][..],
        ),
        (
            "--candidate-repo",
            &[
                "ab-prepare",
                "--candidate-repo",
                "first",
                "--candidate-repo",
                "second",
            ][..],
        ),
        (
            "--work-root",
            &[
                "ab-prepare",
                "--work-root",
                "first",
                "--work-root",
                "second",
            ][..],
        ),
        (
            "--manifest",
            &["ab-prepare", "--manifest", "first", "--manifest", "second"][..],
        ),
        (
            "--reuse-work-root",
            &["ab-prepare", "--reuse-work-root", "--reuse-work-root"][..],
        ),
        (
            "--report",
            &["ab-compare", "--report", "first", "--report", "second"][..],
        ),
        (
            "--code-mode-host",
            &[
                "ab-worker",
                "--code-mode-host",
                "first",
                "--code-mode-host",
                "second",
            ][..],
        ),
        (
            "--variant",
            &["ab-worker", "--variant", "A", "--variant", "B"][..],
        ),
        (
            "--cluster",
            &["ab-worker", "--cluster", "1", "--cluster", "2"][..],
        ),
    ];

    for (flag, arguments) in cases {
        let error = match parse_command_from(strings(arguments)) {
            Ok(_) => panic!("duplicate {flag} unexpectedly parsed"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), format!("{flag} supplied more than once"));
    }
}

#[test]
fn ab_overlay_request_cache_noninferiority_gates() {
    for workload in [
        AbWorkload::LongHistoryNoToolInitial,
        AbWorkload::LongHistoryToolContinuation,
        AbWorkload::StableContextWarmCache,
        AbWorkload::ContextChangeInvalidation,
    ] {
        let mut clusters = paired_request_cache_clusters(workload, 100, 50);
        let violations = ab_correctness_violations(&clusters, workload.class(), workload);
        assert!(
            violations.is_empty(),
            "{}: {:#?}",
            workload.name(),
            violations
        );

        clusters[0].b_samples[0].provider_input_tokens += 1;
        assert!(
            ab_correctness_violations(&clusters, workload.class(), workload)
                .iter()
                .any(|violation| violation.contains("B:token_usage")),
            "{} must enforce deterministic provider usage",
            workload.name()
        );
        clusters[0].b_samples[0].provider_input_tokens -= 1;
        clusters[0].b_samples[0].tool_router_reuse_count -= 1;
        clusters[0].b_samples[0].tool_router_rebuild_count += 1;
        assert!(
            ab_correctness_violations(&clusters, workload.class(), workload)
                .iter()
                .any(|violation| violation.contains("B:tool_router_reuse")),
            "{} must require candidate router reuse",
            workload.name()
        );
        clusters[0].b_samples[0].tool_router_reuse_count += 1;
        clusters[0].b_samples[0].tool_router_rebuild_count -= 1;
        clusters[0].a_samples[0].serialized_bytes = 100;
        clusters[0].b_samples[0].serialized_bytes = 101;
        assert!(
            ab_correctness_violations(&clusters, workload.class(), workload)
                .iter()
                .any(|violation| violation.contains("request_serialization_noninferiority")),
            "{} must reject larger candidate serialization",
            workload.name()
        );
        clusters[0].b_samples[0].serialized_bytes = 100;
        clusters[0].b_samples[0].request_components[0].envelope_sha256 =
            sha256_bytes(b"candidate omitted request envelope field");
        assert!(
            ab_correctness_violations(&clusters, workload.class(), workload)
                .iter()
                .any(|violation| violation.contains("request_serialization_noninferiority")),
            "{} must preserve non-prompt request fields",
            workload.name()
        );
        clusters[0].b_samples[0].request_components[0].envelope_sha256 = clusters[0].a_samples[0]
            .request_components[0]
            .envelope_sha256
            .clone();
        clusters[0].b_samples[0].request_components[0].current_input_sha256 =
            sha256_bytes(b"candidate changed current input");
        assert!(
            ab_correctness_violations(&clusters, workload.class(), workload)
                .iter()
                .any(|violation| violation.contains("request_serialization_noninferiority")),
            "{} must preserve the current input identity",
            workload.name()
        );
        clusters[0].b_samples[0].request_components[0].current_input_sha256 =
            clusters[0].a_samples[0].request_components[0]
                .current_input_sha256
                .clone();
        clusters[0].a_samples[0].prompt_input_tokens = 100;
        clusters[0].b_samples[0].prompt_input_tokens = 101;
        assert!(
            ab_correctness_violations(&clusters, workload.class(), workload)
                .iter()
                .any(|violation| violation.contains("prompt_input_tokens:B=101>A=100")),
            "{} must reject total prompt growth",
            workload.name()
        );
        clusters[0].a_samples[0].serialized_bytes = 100;
        clusters[0].b_samples[0].serialized_bytes = 90;
        clusters[0].a_samples[0].prompt_input_tokens = 100;
        clusters[0].b_samples[0].prompt_input_tokens = 90;
        clusters[0].b_samples[0].canonical_request_body_sha256[0] =
            sha256_bytes(b"candidate request with smaller tools");
        clusters[0].b_samples[0].request_components[0].tool_schemas_sha256 =
            sha256_bytes(b"smaller candidate tool schemas");
        clusters[0].b_samples[0].prompt_injected_tokens = 1;
        assert!(
            !ab_correctness_violations(&clusters, workload.class(), workload)
                .iter()
                .any(|violation| {
                    violation.contains("request_serialization_noninferiority")
                        || violation.contains("prompt_injected_tokens")
                }),
            "{} must accept smaller serialization and prompt despite injected-context growth",
            workload.name()
        );
        clusters[0].b_samples[0].prompt_instruction_tokens += 1;
        assert!(
            ab_correctness_violations(&clusters, workload.class(), workload)
                .iter()
                .any(|violation| violation.contains("prompt_instruction_tokens")),
            "{} must gate every prompt component",
            workload.name()
        );
        clusters[0].b_samples[0].prompt_instruction_tokens -= 1;
        clusters[0].b_samples[0].tool_closure = None;
        assert!(
            ab_correctness_violations(&clusters, workload.class(), workload)
                .iter()
                .any(|violation| violation.contains("B:tool_closure_missing")),
            "{} must enforce candidate closure",
            workload.name()
        );
    }
}

#[test]
fn ab_overlay_context_change_invalidates_only_current_input() {
    let stable = paired_request_cache_clusters(AbWorkload::StableContextWarmCache, 100, 50);
    assert!(
        ab_correctness_violations(
            &stable,
            AbWorkloadClass::Latency,
            AbWorkload::StableContextWarmCache,
        )
        .is_empty()
    );
    let stable_delta = stable[0].b_samples[0]
        .request_component_delta
        .as_ref()
        .expect("stable sample should report its warm-cache comparison");
    assert!(stable_delta.changed_components.is_empty());
    assert!(request_component_names_match(
        &stable_delta.reused_components,
        &AB_REQUEST_COMPONENT_NAMES,
    ));

    let mut changed = paired_request_cache_clusters(AbWorkload::ContextChangeInvalidation, 100, 50);
    assert!(
        ab_correctness_violations(
            &changed,
            AbWorkloadClass::Latency,
            AbWorkload::ContextChangeInvalidation,
        )
        .is_empty()
    );
    let delta = changed[0].b_samples[0]
        .request_component_delta
        .as_ref()
        .expect("context-change sample should report invalidation");
    assert!(request_component_names_match(
        &delta.changed_components,
        &["current_input"],
    ));
    assert!(request_component_names_match(
        &delta.reused_components,
        &[
            "instructions",
            "tool_schemas",
            "history",
            "prompt_cache_key"
        ],
    ));

    changed[0].b_samples[1].request_components[0].instructions_sha256 =
        sha256_bytes(b"unexpected instructions rebuild");
    assert!(
        ab_correctness_violations(
            &changed,
            AbWorkloadClass::Latency,
            AbWorkload::ContextChangeInvalidation,
        )
        .iter()
        .any(|violation| violation.contains("request_component_delta"))
    );
}

#[test]
fn ab_overlay_controller_routes_high_volume_worker_protocol() {
    let workload = ab_controller_workloads()
        .iter()
        .copied()
        .find(|workload| *workload == AbWorkload::CodeModeHighVolume)
        .expect("matrix should retain high-volume workload");
    assert_eq!(workload, AbWorkload::CodeModeHighVolume);
    assert_eq!(workload.expected_logical_generations(), 32);
    assert_eq!(workload.expected_direct_tool_calls(), 32);
    assert_eq!(workload.expected_nested_tool_calls(), 48);
    assert_eq!(workload.class(), AbWorkloadClass::CorrectnessOnly);
    assert!(!workload.latency_metrics().is_empty());

    let ready = AbWorkerReady {
        kind: "ready".to_string(),
        variant: "A".to_string(),
        cluster: 1,
        workload,
        warmups: AB_WARMUPS,
        samples: AB_ITERATIONS,
        warmup_failures: 1,
        warmup_failure_details: vec![AbWarmupFailure {
            warmup_index: 2,
            failure_codes: vec!["tool_output_count".to_string()],
        }],
    };
    let decoded: AbWorkerReady = serde_json::from_slice(
        &serde_json::to_vec(&ready).expect("worker readiness should serialize"),
    )
    .expect("worker readiness should round-trip");
    assert_eq!(decoded.workload, AbWorkload::CodeModeHighVolume);
    assert_eq!(decoded.warmup_failure_details.len(), 1);
    assert_eq!(decoded.warmup_failure_details[0].warmup_index, 2);
    assert_eq!(
        decoded.warmup_failure_details[0].failure_codes,
        ["tool_output_count"]
    );

    let legacy: AbWorkerReady = serde_json::from_value(serde_json::json!({
        "kind": "ready",
        "variant": "A",
        "cluster": 1,
        "warmups": AB_WARMUPS,
        "samples": AB_ITERATIONS,
        "warmup_failures": 0
    }))
    .expect("legacy worker readiness should deserialize compatibly");
    assert_eq!(legacy.workload, AbWorkload::CodeModeNestedDispatch);
    assert!(legacy.warmup_failure_details.is_empty());

    let command = parse_ab_worker_args_from(strings(&[
        "--code-mode-host",
        "host",
        "--variant",
        "B",
        "--cluster",
        "1",
        "--workload",
        "stable_context_warm_cache",
        "--warmups",
        "3",
        "--samples",
        "30",
    ]))
    .expect("matrix worker route should parse");
    let BenchmarkCommand::AbWorker(args) = command else {
        panic!("expected A/B worker command");
    };
    assert_eq!(args.workload, AbWorkload::StableContextWarmCache);
    assert!(validate_ab_worker_pair_index(0, 0, 30).is_ok());
    assert!(validate_ab_worker_pair_index(1, 0, 30).is_err());
    assert!(validate_ab_worker_pair_index(30, 30, 30).is_err());

    let initial = serde_json::json!({
        "input": [{
            "type": "message",
            "role": "user",
            "content": CODE_MODE_HIGH_VOLUME_PROMPT,
        }]
    });
    assert!(high_volume_request_body_is_initial(&initial));
    let follow_up = serde_json::json!({
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": CODE_MODE_HIGH_VOLUME_PROMPT,
            },
            {"type": "custom_tool_call", "call_id": "outer"},
            {"type": "custom_tool_call_output", "call_id": "outer"},
        ]
    });
    assert!(!high_volume_request_body_is_initial(&follow_up));
    let next_subturn = serde_json::json!({
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": CODE_MODE_HIGH_VOLUME_PROMPT,
            },
            {"type": "custom_tool_call_output", "call_id": "outer"},
            {
                "type": "message",
                "role": "user",
                "content": CODE_MODE_HIGH_VOLUME_PROMPT,
            },
        ]
    });
    assert!(high_volume_request_body_is_initial(&next_subturn));
}

#[test]
fn ab_overlay_canonical_controllable_time_excludes_overlapping_unions() {
    let mut timing = TurnTiming::default();
    timing.exclusive.orchestration_ns = 11;
    timing.exclusive.standalone_work_ns = 13;
    timing.exclusive.finalization_ns = 17;
    timing.unions.model_request_wait_union_ns = 5;
    timing.unions.model_stream_processing_union_ns = 7;
    timing.local.planning_union_ns = 9;
    timing.local.router_build_union_ns = 10;
    timing.local.persistence_union_ns = 19;
    timing.pre_first_model_output = Some(codex_protocol::protocol::TurnTimingPreFirstModelOutput {
        captured_at_ns: 101,
        first_request_dispatch_ready_ns: 41,
        client_critical_path_ns: 43,
        attributed_client_union_ns: 47,
        unattributed_pre_output_ns: 53,
        history_snapshot_ns: 59,
        normalization_ns: 61,
        prompt_construction_ns: 67,
        request_transformation_ns: 71,
        serialization_ns: 73,
        transport_readiness_ns: 79,
    });
    timing.unions.model_active_union_ns = 1_000;
    timing.unions.tool_active_union_ns = 2_000;
    timing.counters.tool_output_truncation_count = 1;
    timing.counters.tool_output_projected_token_count = 23;
    timing.counters.tool_output_canonical_byte_count = 29;
    timing.counters.tool_output_canonical_token_count = 31;
    timing.counters.tool_output_model_byte_count = 37;
    timing.counters.tool_output_model_token_count = 23;
    timing.counters.tool_output_artifact_creation_count = 1;
    timing.counters.tool_output_artifact_reuse_count = 2;
    timing.counters.tool_output_artifact_reread_count = 3;
    timing.counters.tool_output_projection_truncation_count = 1;
    timing.counters.tool_output_omitted_section_count = 5;
    timing.counters.tool_output_recovery_call_count = 7;
    timing.counters.tool_output_recovery_retruncation_count = 11;
    timing.counters.tool_output_recursive_spill_count = 13;
    timing.tool_calls.push(TurnTimingToolCall {
        source: TurnTimingToolCallSource::Direct,
        output_projection_ms: Some(17),
        ..TurnTimingToolCall::default()
    });
    let sample = sample_from_timing(&timing);
    assert_eq!(sample.controllable_duration_ns, 41);
    assert_eq!(sample.model_wait_ns, 0);
    assert_eq!(sample.model_request_wait_ns, 5);
    assert_eq!(sample.model_stream_processing_ns, 7);
    assert_eq!(sample.tool_active_ns, 2_000);
    assert_eq!(sample.planning_ns, 9);
    assert_eq!(sample.router_build_ns, 10);
    assert_eq!(sample.persistence_union_ns, 19);
    assert_eq!(sample.first_request_dispatch_ready_ns, 41);
    assert_eq!(sample.pre_first_client_critical_path_ns, 43);
    assert_eq!(sample.pre_first_attributed_client_union_ns, 47);
    assert_eq!(sample.pre_first_unattributed_ns, 53);
    assert_eq!(sample.history_snapshot_ns, 59);
    assert_eq!(sample.normalization_ns, 61);
    assert_eq!(sample.prompt_construction_ns, 67);
    assert_eq!(sample.request_transformation_ns, 71);
    assert_eq!(sample.serialization_ns, 73);
    assert_eq!(sample.transport_readiness_ns, 79);
    assert_eq!(sample.output_projection_count, 1);
    assert_eq!(sample.output_truncation_count, 1);
    assert_eq!(sample.output_projected_token_count, 23);
    assert_eq!(sample.output_canonical_byte_count, 29);
    assert_eq!(sample.output_canonical_token_count, 31);
    assert_eq!(sample.output_model_byte_count, 37);
    assert_eq!(sample.output_model_token_count, 23);
    assert_eq!(sample.output_artifact_creation_count, 1);
    assert_eq!(sample.output_artifact_reuse_count, 2);
    assert_eq!(sample.output_artifact_reread_count, 3);
    assert_eq!(sample.output_projection_truncation_count, 1);
    assert_eq!(sample.output_omitted_section_count, 5);
    assert_eq!(sample.output_recovery_count, 7);
    assert_eq!(sample.output_recovery_retruncation_count, 11);
    assert_eq!(sample.output_recursive_spill_count, 13);

    let serialized = serde_json::to_value(&sample).expect("sample should serialize");
    assert_eq!(serialized["persistence_union_ns"], 19);
    assert_eq!(serialized["history_snapshot_ns"], 59);
    assert_eq!(serialized["transport_readiness_ns"], 79);
    assert_eq!(serialized["output_canonical_byte_count"], 29);
    assert_eq!(serialized["output_model_token_count"], 23);
    assert_eq!(serialized["output_artifact_reuse_count"], 2);
    assert_eq!(serialized["output_recovery_retruncation_count"], 11);

    let mut aggregate = None;
    merge_high_volume_sample(&mut aggregate, sample.clone());
    merge_high_volume_sample(&mut aggregate, sample);
    let aggregate = aggregate.expect("high-volume sample should aggregate");
    assert_eq!(aggregate.persistence_union_ns, 38);
    assert_eq!(aggregate.output_canonical_byte_count, 58);
    assert_eq!(aggregate.output_recursive_spill_count, 26);
}

#[test]
fn ab_overlay_report_shards_and_payload_hash_are_stable() {
    let clusters = paired_clusters(100, 50);
    let encoded = serde_json::to_vec(&clusters).expect("serialize report shards");
    let first_hash = sha256_bytes(&encoded);
    assert_eq!(first_hash, sha256_bytes(&encoded));
    let mut changed = clusters.clone();
    changed[0].a_samples[0].duration_ns += 1;
    assert_ne!(
        first_hash,
        sha256_bytes(&serde_json::to_vec(&changed).unwrap())
    );
    assert_eq!(clusters.len(), AB_CLUSTERS);
    assert!(
        clusters
            .iter()
            .all(|cluster| cluster.a_samples.len() == AB_ITERATIONS)
    );
}

#[test]
fn ab_overlay_replay_session_audit_provenance_is_exact_and_profile_scoped() {
    let expected = replay_session_audit_evidence();
    assert_eq!(AB_REPORT_SCHEMA_VERSION, 20);
    assert_eq!(
        expected.schema_version,
        AB_REPLAY_SESSION_AUDIT_EVIDENCE_VERSION
    );
    assert_eq!(expected.audited_turns, 13);
    assert_eq!(expected.active_seconds, 1_457);
    assert_eq!(expected.logical_generations, 83);
    assert_eq!(expected.orchestration_seconds, 876);
    assert_eq!(expected.model_seconds, 549);
    assert_eq!(expected.input_tokens, 1_720_000);
    assert!(expected.input_tokens_approximate);
    assert_eq!(expected.nonprogress_tokens, 260_000);
    assert!(expected.nonprogress_tokens_approximate);
    assert_eq!(expected.first_response_targeted_actions, 0);

    let serialized = serde_json::to_value(&expected).expect("serialize audit evidence");
    assert_eq!(serialized["input_tokens_approximate"], true);
    assert_eq!(serialized["nonprogress_tokens_approximate"], true);

    let mut provenance = accepted_batch_report_for_import().provenance;
    validate_replay_session_audit_provenance(&provenance)
        .expect("non-replay provenance without replay evidence should be valid");
    provenance.execution_profile = AbExecutionProfile::Replay;
    assert!(validate_replay_session_audit_provenance(&provenance).is_err());

    provenance.replay_session_audit = Some(expected);
    validate_replay_session_audit_provenance(&provenance)
        .expect("replay provenance should require the exact evidence block");
    provenance.execution_profile = AbExecutionProfile::Batch;
    assert!(validate_replay_session_audit_provenance(&provenance).is_err());

    provenance.execution_profile = AbExecutionProfile::Replay;
    provenance
        .replay_session_audit
        .as_mut()
        .expect("replay evidence")
        .logical_generations += 1;
    assert!(validate_replay_session_audit_provenance(&provenance).is_err());
}

#[test]
fn ab_plain_language_report_uses_readable_units_and_explains_technical_metrics() {
    assert_eq!(format_duration_ns_for_humans(999_000_000.0), "999.00 ms");
    assert_eq!(format_duration_ns_for_humans(1_000_000_000.0), "1.00 s");
    assert_eq!(format_duration_ns_for_humans(1_250_000_000.0), "1.25 s");

    assert_eq!(
        ab_metric_plain_language_heading("end_to_end"),
        "  Technical metric: end_to_end — Plain language: the whole turn from start to finish."
    );
    assert!(
        ab_latency_gate_expectation(AbExecutionProfile::Replay, AbLatencyGateMode::Hard)
            .contains("candidate time <= 75% of baseline")
    );

    assert_eq!(
        ab_plain_language_report_path(Path::new("report.json")),
        PathBuf::from("report.txt")
    );
    assert_eq!(
        ab_plain_language_report_path(Path::new("report.txt")),
        PathBuf::from("report.txt.plain.txt")
    );
}

#[test]
fn ab_overlay_imports_only_verified_accepted_reports() {
    let temp = tempfile::tempdir().expect("temporary import root");
    let source = temp.path().join("accepted.json");
    let repo = temp.path().join("repo");
    let report = accepted_batch_report_for_import();
    let bytes = serde_json::to_vec_pretty(&report).expect("encode accepted report");
    fs::write(&source, &bytes).expect("write accepted report fixture");

    let receipt = import_accepted_ab_report(&AbImportReportArgs {
        report: source,
        repo: repo.clone(),
    })
    .expect("verified report should import");
    assert_eq!(receipt.execution_profile, AbExecutionProfile::Batch);
    assert_eq!(receipt.file_sha256, sha256_bytes(&bytes));
    assert_eq!(fs::read(&receipt.destination).unwrap(), bytes);
    assert!(
        receipt
            .destination
            .starts_with(repo.join("docs/benchmarks/turn-latency/accepted"))
    );

    let mut stale_schema = accepted_batch_report_for_import();
    stale_schema.schema_version = AB_REPORT_SCHEMA_VERSION - 1;
    stale_schema.report_payload_sha256.clear();
    stale_schema.report_payload_sha256 =
        sha256_bytes(&serde_json::to_vec(&stale_schema).expect("hash stale-schema fixture"));
    let stale_schema_source = temp.path().join("stale-schema.json");
    fs::write(
        &stale_schema_source,
        serde_json::to_vec_pretty(&stale_schema).expect("encode stale-schema fixture"),
    )
    .expect("write stale-schema report fixture");
    let stale_schema_error = import_accepted_ab_report(&AbImportReportArgs {
        report: stale_schema_source,
        repo: repo.clone(),
    })
    .expect_err("stale report schemas must not import");
    assert!(
        stale_schema_error
            .to_string()
            .contains("accepted report schema"),
        "{stale_schema_error:#}"
    );

    let mut replay = accepted_batch_report_for_import();
    replay.provenance.execution_profile = AbExecutionProfile::Replay;
    replay.report_payload_sha256.clear();
    replay.report_payload_sha256 =
        sha256_bytes(&serde_json::to_vec(&replay).expect("hash replay fixture"));
    let replay_source = temp.path().join("replay.json");
    fs::write(
        &replay_source,
        serde_json::to_vec_pretty(&replay).expect("encode replay fixture"),
    )
    .expect("write replay report fixture");
    let replay_error = import_accepted_ab_report(&AbImportReportArgs {
        report: replay_source,
        repo: repo.clone(),
    })
    .expect_err("replay reports must never enter the tracked accepted set");
    assert!(
        replay_error
            .to_string()
            .contains("only accepted batch or final reports may be imported")
    );

    let mut rejected = accepted_batch_report_for_import();
    rejected.cap_expired = true;
    rejected.report_payload_sha256.clear();
    rejected.report_payload_sha256 =
        sha256_bytes(&serde_json::to_vec(&rejected).expect("hash rejected fixture"));
    let rejected_source = temp.path().join("rejected.json");
    fs::write(
        &rejected_source,
        serde_json::to_vec_pretty(&rejected).expect("encode rejected fixture"),
    )
    .expect("write rejected report fixture");
    assert!(
        import_accepted_ab_report(&AbImportReportArgs {
            report: rejected_source,
            repo,
        })
        .is_err(),
        "capped reports must never enter the tracked accepted set"
    );

    let command = parse_command_from(strings(&[
        "ab-import-report",
        "--report",
        "report.json",
        "--repo",
        "repo",
    ]))
    .expect("accepted-report import command should parse");
    let BenchmarkCommand::AbImportReport(args) = command else {
        panic!("expected accepted-report import command");
    };
    assert_eq!(args.report, PathBuf::from("report.json"));
    assert_eq!(args.repo, PathBuf::from("repo"));
}

#[test]
fn accepted_report_install_is_atomic_idempotent_and_never_clobbers() {
    let temp = tempfile::tempdir().expect("temporary accepted-report root");
    let destination = temp.path().join("accepted").join("batch-report.json");

    install_accepted_ab_report(&destination, b"first").expect("first install should succeed");
    install_accepted_ab_report(&destination, b"first")
        .expect("byte-identical install should be idempotent");
    let error = install_accepted_ab_report(&destination, b"second")
        .expect_err("accepted reports must never be overwritten");

    assert!(error.to_string().contains("different bytes"));
    assert_eq!(fs::read(destination).unwrap(), b"first");
}

#[test]
fn ab_overlay_import_rejects_rehashed_report_with_failing_raw_sample() {
    let temp = tempfile::tempdir().expect("temporary import root");
    let source = temp.path().join("forged-passed.json");
    let mut report = accepted_batch_report_for_import();
    let sample = &mut report.workloads[0].clusters[0].b_samples[0];
    sample.failed = true;
    sample
        .failure_codes
        .push("forged_candidate_failure".to_string());
    rehash_accepted_report(&mut report);
    fs::write(
        &source,
        serde_json::to_vec_pretty(&report).expect("encode forged report"),
    )
    .expect("write forged report");

    let error = import_accepted_ab_report(&AbImportReportArgs {
        report: source,
        repo: temp.path().join("repo"),
    })
    .expect_err("raw sample correctness must outrank self-declared passed fields");
    assert!(
        error.to_string().contains("correctness verdict mismatch"),
        "unexpected import rejection: {error:#}"
    );
}

#[test]
fn accepted_workload_rejects_samples_collected_after_a_terminal_look() {
    const TEST_LOOKS: [usize; 2] = [2, 4];
    let workload = AbWorkload::LongHistoryNoToolInitial;
    let config = AbExecutionConfig {
        profile: AbExecutionProfile::Final,
        warmups: AB_WARMUPS,
        clusters: AB_CLUSTERS,
        looks: &TEST_LOOKS,
        cap: Duration::from_secs(30 * 60),
        latency_hard_gate: true,
    };
    let stopped_at_pairs_per_cluster = TEST_LOOKS[1];
    let clusters = paired_request_cache_clusters(workload, 100, 50);
    let sequential_looks = TEST_LOOKS
        .into_iter()
        .map(|pairs_per_cluster| {
            let prefixes = ab_cluster_prefixes(&clusters, pairs_per_cluster)
                .expect("prefix accepted workload fixture");
            let verdict = evaluate_ab_workload_with_config(
                &prefixes,
                workload.class(),
                workload,
                config,
                pairs_per_cluster,
            )
            .expect("evaluate accepted workload fixture");
            assert_eq!(verdict.decision, AbSequentialDecision::Passed);
            AbSequentialLook {
                pairs_per_cluster,
                total_pairs: pairs_per_cluster * config.clusters,
                ucb_quantile: config.ucb_quantile(),
                latency_gates: verdict.latency_gates,
                latency_diagnostics: verdict.latency_diagnostics,
                correctness_violations: verdict.correctness_violations,
                decision: verdict.decision,
                stop_reason: verdict.stop_reason,
                passed: verdict.passed,
            }
        })
        .collect::<Vec<_>>();
    let final_look = sequential_looks.last().expect("final look");
    let mut report = AbWorkloadReport {
        workload: workload.name().to_string(),
        workload_class: workload.class(),
        workload_shape: workload.report_shape(),
        fixture_sha256: ab_fixture_hash(workload),
        workload_schema_sha256: ab_workload_schema_hash(workload),
        clusters,
        latency_gates: final_look.latency_gates.clone(),
        latency_diagnostics: final_look.latency_diagnostics.clone(),
        latency_gate_mode: config.latency_gate_mode(workload.class()),
        correctness_violations: final_look.correctness_violations.clone(),
        status: AbRunStatus::Passed,
        stop_reason: final_look.stop_reason,
        cap_expired: false,
        stopped_at_pairs_per_cluster,
        passed: true,
        sequential_looks,
        report_payload_sha256: String::new(),
    };
    report.report_payload_sha256 =
        sha256_bytes(&serde_json::to_vec(&report).expect("hash accepted workload fixture"));

    let error = validate_accepted_ab_workload(&mut report, workload, config)
        .expect_err("capture must stop after the first terminal sequential decision");
    assert!(
        error
            .to_string()
            .contains("continued after a terminal look"),
        "unexpected validation error: {error:#}"
    );
}

#[test]
fn ab_overlay_import_rejects_rehashed_report_with_forged_pair_order() {
    let temp = tempfile::tempdir().expect("temporary import root");
    let source = temp.path().join("forged-order.json");
    let mut report = accepted_batch_report_for_import();
    report.workloads[0].clusters[0].a_first[0] = !report.workloads[0].clusters[0].a_first[0];
    rehash_accepted_report(&mut report);
    fs::write(
        &source,
        serde_json::to_vec_pretty(&report).expect("encode forged order report"),
    )
    .expect("write forged order report");

    let error = import_accepted_ab_report(&AbImportReportArgs {
        report: source,
        repo: temp.path().join("repo"),
    })
    .expect_err("declared A/B order must match the controller schedule");
    assert!(
        error.to_string().contains("invalid A/B sample order"),
        "unexpected import rejection: {error:#}"
    );
}

#[test]
fn ab_overlay_import_rejects_noncanonical_unknown_report_fields() {
    let temp = tempfile::tempdir().expect("temporary import root");
    let source = temp.path().join("unknown-field.json");
    let report = accepted_batch_report_for_import();
    let mut value = serde_json::to_value(report).expect("encode report value");
    value
        .as_object_mut()
        .expect("report object")
        .insert("unverifiedNote".to_string(), serde_json::json!("forged"));
    fs::write(
        &source,
        serde_json::to_vec_pretty(&value).expect("encode report with unknown field"),
    )
    .expect("write report with unknown field");

    let error = import_accepted_ab_report(&AbImportReportArgs {
        report: source,
        repo: temp.path().join("repo"),
    })
    .expect_err("unknown report fields must not bypass the report schema");
    assert!(
        error.to_string().contains("canonical benchmark artifact"),
        "unexpected import rejection: {error:#}"
    );
}
