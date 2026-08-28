// A/B statistics, build isolation, worker orchestration, and report verification.
//
// This file is included into the parent benchmark module so its private,
// benchmark-only contracts remain unchanged.

#[derive(Clone, Copy, Debug)]
enum AbLatencyMetric {
    ControllableTurn,
    Preparation,
    PreFirstOutput,
    SamplingToCall,
    PostToolHandoff,
    ParallelGateWait,
    WorkspaceEvidence,
    ProjectionPersistence,
    Terminalization,
}

impl AbLatencyMetric {
    const ALL: [Self; 5] = [
        Self::ControllableTurn,
        Self::Preparation,
        Self::PreFirstOutput,
        Self::SamplingToCall,
        Self::PostToolHandoff,
    ];
    const WITH_PARALLEL_GATE_WAIT: [Self; 6] = [
        Self::ControllableTurn,
        Self::Preparation,
        Self::PreFirstOutput,
        Self::SamplingToCall,
        Self::PostToolHandoff,
        Self::ParallelGateWait,
    ];
    const REQUEST_ONLY: [Self; 3] = [
        Self::ControllableTurn,
        Self::Preparation,
        Self::PreFirstOutput,
    ];
    const REPLAY: [Self; 8] = [
        Self::ControllableTurn,
        Self::Preparation,
        Self::SamplingToCall,
        Self::PostToolHandoff,
        Self::ParallelGateWait,
        Self::WorkspaceEvidence,
        Self::ProjectionPersistence,
        Self::Terminalization,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::ControllableTurn => "controllable_turn",
            Self::Preparation => "request_preparation",
            Self::PreFirstOutput => "pre_first_output",
            Self::SamplingToCall => "sampling_to_call",
            Self::PostToolHandoff => "post_tool_handoff",
            Self::ParallelGateWait => "parallel_gate_wait",
            Self::WorkspaceEvidence => "workspace_evidence",
            Self::ProjectionPersistence => "projection_persistence",
            Self::Terminalization => "terminalization",
        }
    }

    fn value(self, sample: &Sample) -> u64 {
        match self {
            Self::ControllableTurn => sample.controllable_duration_ns,
            Self::Preparation => sample.preparation_ns,
            Self::PreFirstOutput => sample.pre_first_output_ns,
            Self::SamplingToCall => sample.sampling_to_call_ns,
            Self::PostToolHandoff => sample.post_tool_handoff_ns,
            Self::ParallelGateWait => sample.parallel_gate_wait_ns,
            Self::WorkspaceEvidence => sample
                .workspace_evidence_before_ns
                .saturating_add(sample.workspace_evidence_after_ns),
            Self::ProjectionPersistence => sample.persistence_union_ns,
            Self::Terminalization => sample.finalization_ns,
        }
    }
}

fn hierarchical_paired_bootstrap_for_shape(
    clusters: &[AbPairedCluster],
    metric: AbLatencyMetric,
    expected_clusters: usize,
    pairs_per_cluster: usize,
    lcb_quantile: f64,
    ucb_quantile: f64,
) -> Result<AbLatencyGate> {
    anyhow::ensure!(
        (0.0..1.0).contains(&ucb_quantile),
        "bootstrap UCB quantile must be between zero and one"
    );
    anyhow::ensure!(
        (0.0..1.0).contains(&lcb_quantile) && lcb_quantile < ucb_quantile,
        "bootstrap LCB quantile must be between zero and the UCB quantile"
    );
    anyhow::ensure!(
        clusters.len() == expected_clusters,
        "{} requires exactly {expected_clusters} clusters, got {}",
        metric.name(),
        clusters.len()
    );
    for cluster in clusters {
        anyhow::ensure!(
            cluster.a_samples.len() == pairs_per_cluster
                && cluster.b_samples.len() == pairs_per_cluster,
            "{} cluster {} requires exactly {pairs_per_cluster} paired samples",
            metric.name(),
            cluster.cluster
        );
        for (index, (a, b)) in cluster.a_samples.iter().zip(&cluster.b_samples).enumerate() {
            anyhow::ensure!(
                metric.value(a) > 0,
                "{} cluster {} pair {} has a zero A duration",
                metric.name(),
                cluster.cluster,
                index
            );
            anyhow::ensure!(
                metric.value(b) > 0,
                "{} cluster {} pair {} has a zero B duration",
                metric.name(),
                cluster.cluster,
                index
            );
        }
    }

    let all_a = clusters
        .iter()
        .flat_map(|cluster| cluster.a_samples.iter())
        .map(|sample| metric.value(sample) as f64)
        .collect::<Vec<_>>();
    let all_b = clusters
        .iter()
        .flat_map(|cluster| cluster.b_samples.iter())
        .map(|sample| metric.value(sample) as f64)
        .collect::<Vec<_>>();
    let point_median_ratio = percentile(&all_b, 0.5) / percentile(&all_a, 0.5);
    let point_p95_ratio = percentile(&all_b, 0.95) / percentile(&all_a, 0.95);

    let metric_seed = metric.name().bytes().fold(AB_BOOTSTRAP_SEED, |seed, byte| {
        seed.rotate_left(5) ^ u64::from(byte)
    });
    let mut rng = StdRng::seed_from_u64(metric_seed);
    let mut median_ratios = Vec::with_capacity(AB_BOOTSTRAP_REPLICATES);
    let mut p95_ratios = Vec::with_capacity(AB_BOOTSTRAP_REPLICATES);
    for _ in 0..AB_BOOTSTRAP_REPLICATES {
        let mut resampled_a = Vec::with_capacity(expected_clusters * pairs_per_cluster);
        let mut resampled_b = Vec::with_capacity(expected_clusters * pairs_per_cluster);
        for _ in 0..expected_clusters {
            let cluster = &clusters[rng.random_range(0..clusters.len())];
            for _ in 0..pairs_per_cluster {
                let pair = rng.random_range(0..cluster.a_samples.len());
                resampled_a.push(metric.value(&cluster.a_samples[pair]) as f64);
                resampled_b.push(metric.value(&cluster.b_samples[pair]) as f64);
            }
        }
        median_ratios.push(percentile(&resampled_b, 0.5) / percentile(&resampled_a, 0.5));
        p95_ratios.push(percentile(&resampled_b, 0.95) / percentile(&resampled_a, 0.95));
    }
    let median_ratio_lcb = percentile(&median_ratios, lcb_quantile);
    let p95_ratio_lcb = percentile(&p95_ratios, lcb_quantile);
    let median_ratio_ucb = percentile(&median_ratios, ucb_quantile);
    let p95_ratio_ucb = percentile(&p95_ratios, ucb_quantile);
    Ok(AbLatencyGate {
        metric: metric.name().to_string(),
        point_median_ratio,
        point_p95_ratio,
        median_ratio_lcb,
        p95_ratio_lcb,
        median_ratio_ucb,
        p95_ratio_ucb,
        lcb_quantile,
        ucb_quantile,
        pairs_per_cluster,
        median_ratio_ucb_limit: AB_MEDIAN_RATIO_UCB_LIMIT,
        p95_ratio_ucb_limit: AB_P95_RATIO_UCB_LIMIT,
        target_ratio: AB_RATIO_TARGET,
        passed: median_ratio_ucb <= AB_MEDIAN_RATIO_UCB_LIMIT
            && p95_ratio_ucb <= AB_P95_RATIO_UCB_LIMIT,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AbExpectedTokenUsage {
    records: u32,
    input: u64,
    cached_input: u64,
    visible_output: u64,
    reasoning: u64,
    total: u64,
    between_tools_peak_input: u64,
}

fn expected_token_usage(sample: &Sample, workload: AbWorkload) -> Option<AbExpectedTokenUsage> {
    let usage = match workload {
        AbWorkload::CodeModeNestedDispatch => return None,
        AbWorkload::LongHistoryNoToolInitial | AbWorkload::ContextChangeInvalidation => {
            AbExpectedTokenUsage {
                records: 1,
                input: 4_096,
                cached_input: 3_072,
                visible_output: 16,
                reasoning: 0,
                total: 4_112,
                between_tools_peak_input: 0,
            }
        }
        AbWorkload::LongHistoryToolContinuation => AbExpectedTokenUsage {
            records: 2,
            input: 8_448,
            cached_input: 6_400,
            visible_output: 40,
            reasoning: 8,
            total: 8_496,
            between_tools_peak_input: 4_352,
        },
        AbWorkload::StableContextWarmCache => AbExpectedTokenUsage {
            records: 1,
            input: 4_096,
            cached_input: 3_584,
            visible_output: 16,
            reasoning: 0,
            total: 4_112,
            between_tools_peak_input: 0,
        },
        AbWorkload::SingleDirectToolCall
        | AbWorkload::ParallelSafeTripleDirect
        | AbWorkload::ExclusiveGateSerialization => AbExpectedTokenUsage {
            records: 2,
            input: 2_304,
            cached_input: 1_792,
            visible_output: 48,
            reasoning: 8,
            total: 2_360,
            between_tools_peak_input: 1_280,
        },
        AbWorkload::CodeModeHighVolume => {
            let tool_generations = AB_HIGH_VOLUME_SUBTURNS as u64;
            let follow_up_generations =
                u64::from(sample.logical_generations).saturating_sub(tool_generations);
            AbExpectedTokenUsage {
                records: sample.logical_generations,
                input: tool_generations
                    .saturating_mul(1_024)
                    .saturating_add(follow_up_generations.saturating_mul(1_280)),
                cached_input: tool_generations
                    .saturating_mul(768)
                    .saturating_add(follow_up_generations.saturating_mul(1_024)),
                visible_output: tool_generations
                    .saturating_mul(48)
                    .saturating_add(follow_up_generations.saturating_mul(8)),
                reasoning: tool_generations.saturating_mul(16),
                total: tool_generations
                    .saturating_mul(1_088)
                    .saturating_add(follow_up_generations.saturating_mul(1_288)),
                between_tools_peak_input: if follow_up_generations == 0 { 0 } else { 1_280 },
            }
        }
        AbWorkload::RetainedExecWriteStdinLifecycle => AbExpectedTokenUsage {
            records: 4,
            input: 5_632,
            cached_input: 4_608,
            visible_output: 64,
            reasoning: 8,
            total: 5_704,
            between_tools_peak_input: 1_792,
        },
        AbWorkload::AbortDirectNestedInFlight => AbExpectedTokenUsage {
            records: 1,
            input: 1_024,
            cached_input: 768,
            visible_output: 24,
            reasoning: 16,
            total: 1_064,
            between_tools_peak_input: 0,
        },
        AbWorkload::AbortRetainedProcess => AbExpectedTokenUsage {
            records: 1,
            input: 1_024,
            cached_input: 768,
            visible_output: 24,
            reasoning: 8,
            total: 1_056,
            between_tools_peak_input: 0,
        },
        AbWorkload::SessionReplay => return None,
    };
    Some(usage)
}

fn token_usage_matches_workload(sample: &Sample, workload: AbWorkload) -> bool {
    let Some(expected) = expected_token_usage(sample, workload) else {
        return true;
    };
    sample.token_usage_records == expected.records
        && sample.missing_token_usage_records == 0
        && sample.provider_input_tokens == expected.input
        && sample.provider_cached_input_tokens == expected.cached_input
        && sample.provider_visible_output_tokens == expected.visible_output
        && sample.provider_reasoning_tokens == expected.reasoning
        && sample.provider_total_tokens == expected.total
        && sample.between_tools_peak_input_tokens == expected.between_tools_peak_input
}

fn request_component_hashes_are_complete(snapshot: &AbRequestComponentSnapshot) -> bool {
    [
        snapshot.envelope_sha256.as_str(),
        snapshot.instructions_sha256.as_str(),
        snapshot.tool_schemas_sha256.as_str(),
        snapshot.history_sha256.as_str(),
        snapshot.current_input_sha256.as_str(),
        snapshot.prompt_cache_key_sha256.as_str(),
    ]
    .into_iter()
    .all(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn request_serialization_is_noninferior(a: &Sample, b: &Sample) -> bool {
    if a.request_components.len() != b.request_components.len()
        || a.canonical_request_body_sha256.len() != b.canonical_request_body_sha256.len()
        || b.serialized_bytes > a.serialized_bytes
        || b.prompt_input_tokens > a.prompt_input_tokens
    {
        return false;
    }

    let semantic_components_match = a.request_components.iter().zip(&b.request_components).all(
        |(a_component, b_component)| {
            a_component.stage == b_component.stage
                && a_component.envelope_sha256 == b_component.envelope_sha256
                && a_component.instructions_sha256 == b_component.instructions_sha256
                && a_component.current_input_sha256 == b_component.current_input_sha256
                && a_component.prompt_cache_key_sha256 == b_component.prompt_cache_key_sha256
        },
    );
    if !semantic_components_match {
        return false;
    }

    if a.canonical_request_body_sha256 == b.canonical_request_body_sha256 {
        return true;
    }

    b.serialized_bytes < a.serialized_bytes
        && b.prompt_input_tokens < a.prompt_input_tokens
        && a.canonical_request_body_sha256
            .iter()
            .zip(&b.canonical_request_body_sha256)
            .zip(a.request_components.iter().zip(&b.request_components))
            .all(|((a_body, b_body), (a_component, b_component))| {
                a_body == b_body
                    || a_component.tool_schemas_sha256 != b_component.tool_schemas_sha256
            })
}

fn request_component_names_match(actual: &[String], expected: &[&str]) -> bool {
    actual
        .iter()
        .map(String::as_str)
        .eq(expected.iter().copied())
}

fn request_components_match_workload(sample: &Sample, workload: AbWorkload) -> bool {
    if matches!(
        workload,
        AbWorkload::CodeModeNestedDispatch
            | AbWorkload::SingleDirectToolCall
            | AbWorkload::ParallelSafeTripleDirect
            | AbWorkload::ExclusiveGateSerialization
            | AbWorkload::CodeModeHighVolume
            | AbWorkload::RetainedExecWriteStdinLifecycle
            | AbWorkload::AbortDirectNestedInFlight
            | AbWorkload::AbortRetainedProcess
    ) {
        return sample.request_components.is_empty()
            && sample.canonical_request_body_sha256.is_empty()
            && sample.request_component_delta.is_none()
            && sample.history_seed_turns_visible == 0;
    }
    if workload == AbWorkload::SessionReplay {
        return sample.request_components.is_empty()
            && sample.canonical_request_body_sha256.is_empty()
            && sample.request_component_delta.is_none()
            && sample.history_seed_turns_visible == AB_LONG_HISTORY_TURNS as u32;
    }
    if sample.history_seed_turns_visible != AB_LONG_HISTORY_TURNS as u32
        || sample.request_components.len() != workload.expected_logical_generations() as usize
        || sample.canonical_request_body_sha256.len()
            != workload.expected_logical_generations() as usize
        || sample
            .canonical_request_body_sha256
            .iter()
            .any(|hash| hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        || sample
            .request_components
            .iter()
            .any(|snapshot| !request_component_hashes_are_complete(snapshot))
        || sample
            .request_components
            .first()
            .map(|snapshot| snapshot.stage.as_str())
            != Some("initial")
    {
        return false;
    }
    match workload {
        AbWorkload::LongHistoryNoToolInitial => sample.request_component_delta.is_none(),
        AbWorkload::LongHistoryToolContinuation => {
            let Some(continuation) = sample.request_components.get(1) else {
                return false;
            };
            if continuation.stage != "continuation" || sample.request_component_delta.is_some() {
                return false;
            }
            let delta = request_component_delta(&sample.request_components[0], continuation);
            request_component_names_match(&delta.changed_components, &["history"])
                && request_component_names_match(
                    &delta.reused_components,
                    &[
                        "instructions",
                        "tool_schemas",
                        "current_input",
                        "prompt_cache_key",
                    ],
                )
        }
        AbWorkload::StableContextWarmCache => {
            sample
                .request_component_delta
                .as_ref()
                .is_some_and(|delta| {
                    delta.compared_to_previous
                        && delta.changed_components.is_empty()
                        && request_component_names_match(
                            &delta.reused_components,
                            &AB_REQUEST_COMPONENT_NAMES,
                        )
                })
        }
        AbWorkload::ContextChangeInvalidation => sample
            .request_component_delta
            .as_ref()
            .is_some_and(|delta| {
                delta.compared_to_previous
                    && request_component_names_match(&delta.changed_components, &["current_input"])
                    && request_component_names_match(
                        &delta.reused_components,
                        &[
                            "instructions",
                            "tool_schemas",
                            "history",
                            "prompt_cache_key",
                        ],
                    )
            }),
        AbWorkload::CodeModeNestedDispatch
        | AbWorkload::SingleDirectToolCall
        | AbWorkload::ParallelSafeTripleDirect
        | AbWorkload::ExclusiveGateSerialization
        | AbWorkload::CodeModeHighVolume
        | AbWorkload::RetainedExecWriteStdinLifecycle
        | AbWorkload::AbortDirectNestedInFlight
        | AbWorkload::AbortRetainedProcess
        | AbWorkload::SessionReplay => unreachable!(),
    }
}

fn record_retained_lifecycle_coverage_failures(sample: &mut Sample) {
    if sample.incomplete_lifecycle_calls != 0 {
        sample
            .failure_codes
            .push("incomplete_tool_lifecycle".to_string());
    }
    if !sample.lifecycle_complete {
        sample.failure_codes.push("lifecycle_coverage".to_string());
    }
    if !sample.latency_eligible {
        sample.failure_codes.push("latency_ineligible".to_string());
    }
}

fn retained_exec_lifecycle_matches(sample: &Sample) -> bool {
    sample.terminal_event == "turn_complete"
        && sample.completion_status.as_deref() == Some("not_applicable")
        && sample.abort_reason.is_none()
        && sample.typed_error_count == 0
        && sample.final_response_present
        && sample.retained_write_stdin_poll_count == 2
        && sample.retained_session_ids.len() == 2
        && !sample.retained_session_ids[0].is_empty()
        && sample.retained_session_ids[0] == sample.retained_session_ids[1]
        && sample.retained_process_exit_observed
        && sample.retained_process_cleanup_complete
        && sample.expected_retained_processes == 1
        && sample.unexpected_live_processes == 0
        && sample.incomplete_lifecycle_calls == 0
        && sample.incomplete_tool_lifecycles.is_empty()
        && sample.lifecycle_complete
        && sample.latency_eligible
}

fn abort_direct_nested_lifecycle_matches(sample: &Sample) -> bool {
    let direct = sample
        .tool_call_graph
        .iter()
        .find(|call| call.source.as_deref() == Some("direct"));
    let nested = sample
        .tool_call_graph
        .iter()
        .find(|call| call.source.as_deref() == Some("code_mode"));
    let registration_matches = direct.zip(nested).is_some_and(|(direct, nested)| {
        sample.abort_registered_call_ids == [direct.call_id.clone(), nested.call_id.clone()]
            && sample.abort_barrier_call_id.as_deref() == Some(nested.call_id.as_str())
    });
    sample.terminal_event == "turn_aborted"
        && sample.completion_status.is_none()
        && sample.abort_reason.as_deref() == Some("interrupted")
        && sample.typed_error_count == 0
        && !sample.final_response_present
        && !sample.forged_turn_complete_observed
        && registration_matches
        && sample.abort_terminal_outcomes_by_registration.len() == 2
        && sample
            .abort_terminal_outcomes_by_registration
            .iter()
            .all(|outcome| !outcome.is_empty())
        && sample.abort_model_resumed_call_count == 0
        && sample.expected_retained_processes == 0
        && sample.unexpected_live_processes == 0
        && sample.incomplete_lifecycle_calls == 0
        && sample.incomplete_tool_lifecycles.is_empty()
        && sample.lifecycle_complete
        && !sample.latency_eligible
}

fn abort_retained_process_lifecycle_matches(sample: &Sample) -> bool {
    let Some(call) = sample.tool_call_graph.first() else {
        return false;
    };
    sample.terminal_event == "turn_aborted"
        && sample.completion_status.is_none()
        && sample.abort_reason.as_deref() == Some("interrupted")
        && sample.typed_error_count == 0
        && !sample.final_response_present
        && !sample.forged_turn_complete_observed
        && sample.abort_registered_call_ids == [call.call_id.clone()]
        && sample.abort_terminal_outcomes_by_registration.len() == 1
        && !sample.abort_terminal_outcomes_by_registration[0].is_empty()
        && sample.abort_barrier_call_id.as_deref() == Some(call.call_id.as_str())
        && sample.abort_model_resumed_call_count == 0
        && sample.retained_process_owned_before_abort
        && sample.retained_process_count_before_abort == 1
        && sample
            .retained_abort_process_id
            .as_deref()
            .is_some_and(|process_id| !process_id.is_empty())
        && sample.retained_abort_persisted_result_count == 1
        && sample.retained_abort_cancellation_observed
        && sample.retained_process_exit_observed
        && sample.retained_process_cleanup_complete
        && sample.retained_write_stdin_poll_count == 0
        && sample.expected_retained_processes == 0
        && sample.unexpected_live_processes == 0
        && sample.incomplete_lifecycle_calls == 0
        && sample.incomplete_tool_lifecycles.is_empty()
        && sample.lifecycle_complete
        && !sample.latency_eligible
}

fn allowed_baseline_failure_codes(workload: AbWorkload) -> &'static [&'static str] {
    match workload {
        AbWorkload::ParallelSafeTripleDirect => &["tool_gate_execution", "tool_output_count"],
        AbWorkload::ExclusiveGateSerialization => &[
            "tool_gate_execution",
            "tool_output_count",
            "exclusive_child_output",
        ],
        _ => &[],
    }
}

fn unexpected_failure_codes<'a>(
    variant: &str,
    workload: AbWorkload,
    sample: &'a Sample,
) -> Vec<&'a str> {
    if variant == "A" && workload.allows_raw_baseline_behavior() {
        return Vec::new();
    }
    let allowed = if variant == "A" {
        allowed_baseline_failure_codes(workload)
    } else {
        &[]
    };
    sample
        .failure_codes
        .iter()
        .map(String::as_str)
        .filter(|code| !allowed.contains(code))
        .collect()
}

fn ab_correctness_violations_for_shape(
    clusters: &[AbPairedCluster],
    workload_class: AbWorkloadClass,
    workload: AbWorkload,
    expected_clusters: usize,
    pairs_per_cluster: usize,
) -> Vec<String> {
    let mut violations = Vec::new();
    let expected_generations = workload.expected_logical_generations();
    let expected_direct_calls = workload.expected_direct_tool_calls();
    let expected_nested_calls = workload.expected_nested_tool_calls();
    let expected_tool_calls = expected_direct_calls.saturating_add(expected_nested_calls);
    if clusters.len() != expected_clusters {
        violations.push(format!(
            "cluster_count:{}!=expected:{expected_clusters}",
            clusters.len()
        ));
    }
    for cluster in clusters {
        if cluster.b_warmup_failures != 0 {
            violations.push(format!(
                "cluster:{}:warmup_failures:A={}:B={}",
                cluster.cluster, cluster.a_warmup_failures, cluster.b_warmup_failures
            ));
        }
        if cluster.a_samples.len() != pairs_per_cluster
            || cluster.b_samples.len() != pairs_per_cluster
            || cluster.a_first.len() != pairs_per_cluster
        {
            violations.push(format!(
                "cluster:{}:incomplete_pairing:A={}:B={}:order={}",
                cluster.cluster,
                cluster.a_samples.len(),
                cluster.b_samples.len(),
                cluster.a_first.len()
            ));
            continue;
        }
        for index in 0..pairs_per_cluster {
            let a = &cluster.a_samples[index];
            let b = &cluster.b_samples[index];
            for (variant, sample) in [("A", a), ("B", b)] {
                let raw_baseline_behavior =
                    variant == "A" && workload.allows_raw_baseline_behavior();
                let declared_baseline_defect =
                    variant == "A" && !allowed_baseline_failure_codes(workload).is_empty();
                let unexpected_failures = unexpected_failure_codes(variant, workload, sample);
                if !unexpected_failures.is_empty()
                    || (sample.failed && sample.failure_codes.is_empty())
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:failed:{}",
                        cluster.cluster,
                        unexpected_failures.join(",")
                    ));
                }
                if workload_class == AbWorkloadClass::Latency && !sample.latency_eligible {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:coverage_incomplete",
                        cluster.cluster
                    ));
                }
                let generation_graph_matches = match workload {
                    AbWorkload::CodeModeNestedDispatch
                    | AbWorkload::LongHistoryNoToolInitial
                    | AbWorkload::LongHistoryToolContinuation
                    | AbWorkload::StableContextWarmCache
                    | AbWorkload::ContextChangeInvalidation
                    | AbWorkload::SingleDirectToolCall
                    | AbWorkload::ParallelSafeTripleDirect
                    | AbWorkload::ExclusiveGateSerialization
                    | AbWorkload::RetainedExecWriteStdinLifecycle
                    | AbWorkload::AbortDirectNestedInFlight
                    | AbWorkload::AbortRetainedProcess => {
                        sample.logical_generations == expected_generations
                            && sample.provider_attempts == expected_generations
                            && sample.sampling_requests == expected_generations
                    }
                    AbWorkload::CodeModeHighVolume => {
                        sample.logical_generations == expected_generations
                            && sample.provider_attempts == expected_generations
                            && sample.sampling_requests == expected_generations
                            && sample.failure_terminalized_subturns == 0
                    }
                    AbWorkload::SessionReplay => unreachable!(),
                };
                if !generation_graph_matches
                    || sample.retry_attempts != 0
                    || sample.fallback_attempts != 0
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:generation_graph",
                        cluster.cluster
                    ));
                }
                if !token_usage_matches_workload(sample, workload) {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:token_usage",
                        cluster.cluster
                    ));
                }
                if !request_components_match_workload(sample, workload) {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:request_components",
                        cluster.cluster
                    ));
                }
                if workload.is_request_cache() {
                    let router_decisions = sample
                        .tool_router_reuse_count
                        .saturating_add(sample.tool_router_rebuild_count);
                    if router_decisions != sample.logical_generations {
                        violations.push(format!(
                            "cluster:{}:pair:{index}:{variant}:tool_router_decision_coverage",
                            cluster.cluster
                        ));
                    }
                    if variant == "B"
                        && (sample.tool_router_rebuild_count != 0
                            || sample.tool_router_reuse_count != sample.logical_generations)
                    {
                        violations.push(format!(
                            "cluster:{}:pair:{index}:B:tool_router_reuse",
                            cluster.cluster
                        ));
                    }
                }
                if matches!(
                    workload,
                    AbWorkload::LongHistoryNoToolInitial
                        | AbWorkload::LongHistoryToolContinuation
                        | AbWorkload::StableContextWarmCache
                        | AbWorkload::ContextChangeInvalidation
                        | AbWorkload::SingleDirectToolCall
                        | AbWorkload::ParallelSafeTripleDirect
                        | AbWorkload::ExclusiveGateSerialization
                        | AbWorkload::RetainedExecWriteStdinLifecycle
                        | AbWorkload::AbortDirectNestedInFlight
                        | AbWorkload::AbortRetainedProcess
                ) && sample.workload_subturns != 1
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:workload_subturns",
                        cluster.cluster
                    ));
                }
                if !raw_baseline_behavior
                    && (sample.direct_tool_calls != expected_direct_calls
                        || sample.nested_tool_calls != expected_nested_calls
                        || sample.paired_tool_calls != expected_tool_calls
                        || sample.tool_calls != expected_tool_calls
                        || sample.unresolved_tool_calls != 0
                        || sample.orphan_tool_calls != 0)
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:tool_graph",
                        cluster.cluster
                    ));
                }
                if !raw_baseline_behavior && !tool_graph_matches_workload(sample, workload) {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:tool_graph_identity",
                        cluster.cluster
                    ));
                }
                if !raw_baseline_behavior
                    && !declared_baseline_defect
                    && sample.output_projection_count != expected_tool_calls
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:output_projection_count:{}!=expected:{expected_tool_calls}",
                        cluster.cluster, sample.output_projection_count
                    ));
                }
                if workload == AbWorkload::CodeModeHighVolume
                    && sample.max_concurrent_tool_calls < 2
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:multi_call_concurrency",
                        cluster.cluster
                    ));
                }
                if matches!(
                    workload,
                    AbWorkload::SingleDirectToolCall
                        | AbWorkload::ParallelSafeTripleDirect
                        | AbWorkload::ExclusiveGateSerialization
                ) && !declared_baseline_defect
                    && !tool_gate_execution_matches(sample, workload)
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:{}_contract",
                        cluster.cluster,
                        workload.name()
                    ));
                }
                if workload == AbWorkload::RetainedExecWriteStdinLifecycle
                    && !raw_baseline_behavior
                    && !retained_exec_lifecycle_matches(sample)
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:retained_process_lifecycle",
                        cluster.cluster
                    ));
                }
                if workload == AbWorkload::AbortDirectNestedInFlight
                    && !raw_baseline_behavior
                    && !abort_direct_nested_lifecycle_matches(sample)
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:abort_direct_nested_lifecycle",
                        cluster.cluster
                    ));
                }
                if workload == AbWorkload::AbortRetainedProcess
                    && !raw_baseline_behavior
                    && !abort_retained_process_lifecycle_matches(sample)
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:abort_retained_process_lifecycle",
                        cluster.cluster
                    ));
                }
                match sample.tool_closure.as_ref() {
                    Some(closure)
                        if !raw_baseline_behavior
                            && !tool_closure_matches_sample(sample, closure) =>
                    {
                        violations.push(format!(
                            "cluster:{}:pair:{index}:{variant}:tool_closure_mismatch",
                            cluster.cluster
                        ));
                    }
                    None if variant == "B" => {
                        violations.push(format!(
                            "cluster:{}:pair:{index}:B:tool_closure_missing",
                            cluster.cluster
                        ));
                    }
                    _ => {}
                }
                if (!raw_baseline_behavior && sample.unexpected_live_processes != 0)
                    || sample.workspace_evidence_timeouts != 0
                    || sample.output_truncation_count != 0
                    || sample.output_artifact_reread_count != 0
                    || sample.output_projection_truncation_count != 0
                    || sample.output_omitted_section_count != 0
                    || sample.output_recovery_count != 0
                    || sample.output_recovery_retruncation_count != 0
                    || sample.output_recursive_spill_count != 0
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{variant}:unexpected_work",
                        cluster.cluster
                    ));
                }
                if index > 0
                    && matches!(
                        workload,
                        AbWorkload::StableContextWarmCache | AbWorkload::ContextChangeInvalidation
                    )
                {
                    let previous = if variant == "A" {
                        &cluster.a_samples[index - 1]
                    } else {
                        &cluster.b_samples[index - 1]
                    };
                    let observed_delta = previous
                        .request_components
                        .first()
                        .zip(sample.request_components.first())
                        .map(|(previous, current)| request_component_delta(previous, current));
                    if observed_delta.as_ref() != sample.request_component_delta.as_ref() {
                        violations.push(format!(
                            "cluster:{}:pair:{index}:{variant}:request_component_delta",
                            cluster.cluster
                        ));
                    }
                }
            }
            if workload.is_request_cache() && !request_serialization_is_noninferior(a, b)
            {
                violations.push(format!(
                    "cluster:{}:pair:{index}:request_serialization_noninferiority",
                    cluster.cluster
                ));
            }
            if workload != AbWorkload::CodeModeHighVolume
                && !workload.restores_missing_baseline_tool_outputs()
                && b.prompt_input_tokens > a.prompt_input_tokens
            {
                violations.push(format!(
                    "cluster:{}:pair:{index}:prompt_input_tokens:B={}>A={}",
                    cluster.cluster, b.prompt_input_tokens, a.prompt_input_tokens
                ));
            }
            for (name, a_value, b_value) in [
                (
                    "logical_generations",
                    u64::from(a.logical_generations),
                    u64::from(b.logical_generations),
                ),
                (
                    "provider_attempts",
                    u64::from(a.provider_attempts),
                    u64::from(b.provider_attempts),
                ),
                (
                    "retry_attempts",
                    u64::from(a.retry_attempts),
                    u64::from(b.retry_attempts),
                ),
                (
                    "fallback_attempts",
                    u64::from(a.fallback_attempts),
                    u64::from(b.fallback_attempts),
                ),
                (
                    "avoidable_generations",
                    u64::from(a.avoidable_generations),
                    u64::from(b.avoidable_generations),
                ),
                (
                    "nonprogress_tokens",
                    a.nonprogress_tokens,
                    b.nonprogress_tokens,
                ),
                (
                    "between_tools_peak_input_tokens",
                    a.between_tools_peak_input_tokens,
                    b.between_tools_peak_input_tokens,
                ),
                (
                    "provider_input_tokens",
                    a.provider_input_tokens,
                    b.provider_input_tokens,
                ),
                (
                    "prompt_instruction_tokens",
                    a.prompt_instruction_tokens,
                    b.prompt_instruction_tokens,
                ),
                (
                    "prompt_schema_tokens",
                    a.prompt_schema_tokens,
                    b.prompt_schema_tokens,
                ),
                (
                    "prompt_history_tokens",
                    a.prompt_history_tokens,
                    b.prompt_history_tokens,
                ),
                (
                    "prompt_current_input_tokens",
                    a.prompt_current_input_tokens,
                    b.prompt_current_input_tokens,
                ),
                (
                    "prompt_repository_tokens",
                    a.prompt_repository_tokens,
                    b.prompt_repository_tokens,
                ),
                (
                    "prompt_skill_tokens",
                    a.prompt_skill_tokens,
                    b.prompt_skill_tokens,
                ),
                (
                    "prompt_injected_tokens",
                    a.prompt_injected_tokens,
                    b.prompt_injected_tokens,
                ),
                (
                    "repeated_unchanged_context_tokens",
                    a.repeated_unchanged_context_tokens,
                    b.repeated_unchanged_context_tokens,
                ),
                (
                    "convoy_count",
                    u64::from(a.convoy_count),
                    u64::from(b.convoy_count),
                ),
            ] {
                if workload.restores_missing_baseline_tool_outputs()
                    && matches!(name, "prompt_history_tokens" | "prompt_injected_tokens")
                {
                    continue;
                }
                if name == "prompt_injected_tokens"
                    && b.prompt_input_tokens <= a.prompt_input_tokens
                {
                    continue;
                }
                if workload == AbWorkload::CodeModeHighVolume
                    && matches!(
                        name,
                        "prompt_instruction_tokens"
                            | "prompt_schema_tokens"
                            | "prompt_history_tokens"
                            | "prompt_current_input_tokens"
                            | "prompt_repository_tokens"
                            | "prompt_skill_tokens"
                            | "prompt_injected_tokens"
                            | "repeated_unchanged_context_tokens"
                    )
                {
                    continue;
                }
                if b_value > a_value {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{name}:B={b_value}>A={a_value}",
                        cluster.cluster
                    ));
                }
            }
            if workload != AbWorkload::CodeModeHighVolume
                && !workload.restores_missing_baseline_tool_outputs()
                && b.prompt_reconciliation_residual.unsigned_abs()
                    > a.prompt_reconciliation_residual.unsigned_abs()
            {
                violations.push(format!(
                    "cluster:{}:pair:{index}:prompt_reconciliation_residual:B={}>A={}",
                    cluster.cluster,
                    b.prompt_reconciliation_residual.unsigned_abs(),
                    a.prompt_reconciliation_residual.unsigned_abs()
                ));
            }
        }
    }
    type TokenMetric = (&'static str, fn(&Sample) -> u64);
    let token_metrics: [TokenMetric; 2] = [
        ("provider_input_tokens", |sample: &Sample| {
            sample.provider_input_tokens
        }),
        ("between_tools_peak_input_tokens", |sample: &Sample| {
            sample.between_tools_peak_input_tokens
        }),
    ];
    for (name, value) in token_metrics {
        let a = clusters
            .iter()
            .flat_map(|cluster| cluster.a_samples.iter())
            .map(|sample| value(sample) as f64)
            .collect::<Vec<_>>();
        let b = clusters
            .iter()
            .flat_map(|cluster| cluster.b_samples.iter())
            .map(|sample| value(sample) as f64)
            .collect::<Vec<_>>();
        if !a.is_empty() && !b.is_empty() && percentile(&b, 0.95) > percentile(&a, 0.95) {
            violations.push(format!("{name}:p95_increased"));
        }
    }
    violations
}

fn evaluate_ab_workload_with_config(
    clusters: &[AbPairedCluster],
    workload_class: AbWorkloadClass,
    workload: AbWorkload,
    config: AbExecutionConfig,
    pairs_per_cluster: usize,
) -> Result<AbWorkloadVerdict> {
    if config.profile == AbExecutionProfile::Replay {
        anyhow::ensure!(
            workload == AbWorkload::SessionReplay,
            "replay profile only evaluates the session replay workload"
        );
        return evaluate_session_replay(clusters, config, pairs_per_cluster);
    }
    let correctness_violations = ab_correctness_violations_for_shape(
        clusters,
        workload_class,
        workload,
        config.clusters,
        pairs_per_cluster,
    );
    let mut latency_gates = Vec::new();
    let mut latency_diagnostics = Vec::new();
    if workload_class == AbWorkloadClass::Latency && correctness_violations.is_empty() {
        for metric in workload.latency_metrics().iter().copied() {
            match hierarchical_paired_bootstrap_for_shape(
                clusters,
                metric,
                config.clusters,
                pairs_per_cluster,
                config.lcb_quantile(),
                config.ucb_quantile(),
            ) {
                Ok(gate) => latency_gates.push(gate),
                Err(error) => latency_diagnostics.push(format!("{}:{error:#}", metric.name())),
            }
        }
    }
    let expected_latency_gates = match workload_class {
        AbWorkloadClass::Latency => workload.latency_metrics().len(),
        AbWorkloadClass::CorrectnessOnly => 0,
    };
    let latency_contract_complete =
        latency_diagnostics.is_empty() && latency_gates.len() == expected_latency_gates;
    let latency_contract_passed =
        latency_contract_complete && latency_gates.iter().all(|gate| gate.passed);
    let latency_contract_clearly_failed = latency_contract_complete
        && latency_gates.iter().any(|gate| {
            gate.median_ratio_lcb > gate.median_ratio_ucb_limit
                || gate.p95_ratio_lcb > gate.p95_ratio_ucb_limit
        });
    let (decision, stop_reason) = if !correctness_violations.is_empty() {
        (
            AbSequentialDecision::Failed,
            AbStopReason::CorrectnessFailure,
        )
    } else if workload_class == AbWorkloadClass::CorrectnessOnly {
        (
            AbSequentialDecision::Passed,
            AbStopReason::CorrectnessOnlyComplete,
        )
    } else if !config.latency_hard_gate {
        (AbSequentialDecision::Passed, AbStopReason::AdvisoryComplete)
    } else if !latency_diagnostics.is_empty() {
        (AbSequentialDecision::Failed, AbStopReason::LatencyInvalid)
    } else if latency_contract_passed {
        (AbSequentialDecision::Passed, AbStopReason::LatencyClearPass)
    } else if latency_contract_clearly_failed {
        (
            AbSequentialDecision::Failed,
            AbStopReason::LatencyClearFailure,
        )
    } else if pairs_per_cluster == config.max_pairs_per_cluster() {
        (
            AbSequentialDecision::Failed,
            AbStopReason::MaximumLookWithoutPass,
        )
    } else {
        (
            AbSequentialDecision::Continue,
            AbStopReason::LatencyUncertain,
        )
    };
    let passed = decision == AbSequentialDecision::Passed;
    Ok(AbWorkloadVerdict {
        latency_gates,
        latency_diagnostics,
        correctness_violations,
        decision,
        stop_reason,
        passed,
    })
}

fn evaluate_session_replay(
    clusters: &[AbPairedCluster],
    config: AbExecutionConfig,
    pairs_per_cluster: usize,
) -> Result<AbWorkloadVerdict> {
    let mut violations = Vec::new();
    anyhow::ensure!(config.warmups == 0, "replay profile must not warm workers");
    anyhow::ensure!(
        config.clusters == 1,
        "replay profile must use one paired cluster"
    );
    anyhow::ensure!(
        config.looks == AB_REPLAY_LOOKS,
        "replay profile must declare one ten-pair look"
    );
    if clusters.len() != 1 {
        violations.push(format!("cluster_count:{}!=expected:1", clusters.len()));
    }
    for cluster in clusters {
        if cluster.a_warmup_failures != 0 || cluster.b_warmup_failures != 0 {
            violations.push(format!(
                "cluster:{}:unexpected_warmups:A={}:B={}",
                cluster.cluster, cluster.a_warmup_failures, cluster.b_warmup_failures
            ));
        }
        if cluster.a_samples.len() != pairs_per_cluster
            || cluster.b_samples.len() != pairs_per_cluster
            || cluster.a_first.len() != pairs_per_cluster
        {
            violations.push(format!(
                "cluster:{}:incomplete_pairing:A={}:B={}:order={}",
                cluster.cluster,
                cluster.a_samples.len(),
                cluster.b_samples.len(),
                cluster.a_first.len()
            ));
            continue;
        }
        for index in 0..pairs_per_cluster {
            let expected_a_first = index % 2 == 0;
            if cluster.a_first[index] != expected_a_first {
                violations.push(format!(
                    "cluster:{}:pair:{index}:order:{}!=expected:{}",
                    cluster.cluster, cluster.a_first[index], expected_a_first
                ));
            }
            let a = &cluster.a_samples[index];
            let b = &cluster.b_samples[index];
            replay_sample_contract_violations(cluster.cluster, index, "A", a, &mut violations);
            replay_sample_contract_violations(cluster.cluster, index, "B", b, &mut violations);

            if a.logical_generations != AB_REPLAY_A_GENERATIONS
                || a.provider_attempts != AB_REPLAY_A_GENERATIONS
                || a.sampling_requests != AB_REPLAY_A_GENERATIONS
            {
                violations.push(format!(
                    "cluster:{}:pair:{index}:A:generation_graph",
                    cluster.cluster
                ));
            }
            if b.logical_generations != AB_REPLAY_B_GENERATIONS
                || b.provider_attempts != AB_REPLAY_B_GENERATIONS
                || b.sampling_requests != AB_REPLAY_B_GENERATIONS
                || b.retry_attempts != 0
                || b.fallback_attempts != 0
            {
                violations.push(format!(
                    "cluster:{}:pair:{index}:B:generation_graph",
                    cluster.cluster
                ));
            }
            let a_targeted = a.replay_targeted_action.as_ref();
            if a_targeted.is_none_or(|evidence| evidence.generation_index != 1 || evidence.targeted)
            {
                violations.push(format!(
                    "cluster:{}:pair:{index}:A:first_action_not_defective",
                    cluster.cluster
                ));
            }
            let b_targeted = b.replay_targeted_action.as_ref();
            if b_targeted.is_none_or(|evidence| {
                !evidence.action_first_instruction_observed
                    || evidence.generation_index != 1
                    || !evidence.targeted
                    || evidence.action.is_empty()
                    || evidence.exact_target.is_empty()
            }) {
                violations.push(format!(
                    "cluster:{}:pair:{index}:B:first_action_not_targeted",
                    cluster.cluster
                ));
            }
            for purpose in [
                "wait",
                "repair",
                "failure_diagnosis",
                "redundant_continuation",
                "repeated_discovery",
            ] {
                if a.generation_purposes
                    .get(purpose)
                    .copied()
                    .unwrap_or_default()
                    == 0
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:A:missing_purpose:{purpose}",
                        cluster.cluster
                    ));
                }
            }
            if b.avoidable_generations != 0 || b.nonprogress_tokens != 0 {
                violations.push(format!(
                    "cluster:{}:pair:{index}:B:nonprogress",
                    cluster.cluster
                ));
            }
            for purpose in [
                "wait",
                "repair",
                "failure_diagnosis",
                "redundant_continuation",
                "reviewer",
                "proof",
                "compaction",
            ] {
                if b.generation_purposes
                    .get(purpose)
                    .copied()
                    .unwrap_or_default()
                    != 0
                {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:B:avoidable_purpose:{purpose}",
                        cluster.cluster
                    ));
                }
            }
            if a.provider_input_tokens == 0
                || b.provider_input_tokens.saturating_mul(2) > a.provider_input_tokens
            {
                violations.push(format!(
                    "cluster:{}:pair:{index}:provider_input_ratio:B={}>A={}",
                    cluster.cluster, b.provider_input_tokens, a.provider_input_tokens
                ));
            }
            for (name, a_value, b_value) in [
                (
                    "between_tools_peak_input_tokens",
                    a.between_tools_peak_input_tokens,
                    b.between_tools_peak_input_tokens,
                ),
                (
                    "prompt_schema_tokens",
                    a.prompt_schema_tokens,
                    b.prompt_schema_tokens,
                ),
                (
                    "repeated_unchanged_context_tokens",
                    a.repeated_unchanged_context_tokens,
                    b.repeated_unchanged_context_tokens,
                ),
            ] {
                if b_value > a_value {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{name}:B={b_value}>A={a_value}",
                        cluster.cluster
                    ));
                }
            }
            if b.prompt_reconciliation_residual.unsigned_abs()
                > a.prompt_reconciliation_residual.unsigned_abs()
            {
                violations.push(format!(
                    "cluster:{}:pair:{index}:prompt_reconciliation_residual",
                    cluster.cluster
                ));
            }
            for metric in AbLatencyMetric::REPLAY {
                let a_value = metric.value(a);
                let b_value = metric.value(b);
                if a_value == 0 || b_value == 0 {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{}:missing_duration:A={a_value}:B={b_value}",
                        cluster.cluster,
                        metric.name()
                    ));
                } else if b_value.saturating_mul(2) > a_value {
                    violations.push(format!(
                        "cluster:{}:pair:{index}:{}:ratio:B={b_value}>50pct_A={}",
                        cluster.cluster,
                        metric.name(),
                        a_value / 2
                    ));
                }
            }
        }
    }

    let complete = clusters.len() == 1
        && pairs_per_cluster == AB_REPLAY_PAIRS
        && clusters.first().is_some_and(|cluster| {
            cluster.a_samples.len() == AB_REPLAY_PAIRS
                && cluster.b_samples.len() == AB_REPLAY_PAIRS
                && cluster.a_first.len() == AB_REPLAY_PAIRS
        });
    if !complete {
        violations.push(format!(
            "replay_requires_exactly_{AB_REPLAY_PAIRS}_complete_pairs"
        ));
    }

    let latency_gates = if complete {
        AbLatencyMetric::REPLAY
            .iter()
            .copied()
            .map(|metric| replay_descriptive_latency_gate(clusters, metric, pairs_per_cluster))
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let passed = complete && violations.is_empty() && latency_gates.iter().all(|gate| gate.passed);
    Ok(AbWorkloadVerdict {
        latency_gates,
        latency_diagnostics: Vec::new(),
        correctness_violations: violations,
        decision: if passed {
            AbSequentialDecision::Passed
        } else {
            AbSequentialDecision::Failed
        },
        stop_reason: if passed {
            AbStopReason::LatencyClearPass
        } else if complete {
            AbStopReason::CorrectnessFailure
        } else {
            AbStopReason::MaximumLookWithoutPass
        },
        passed,
    })
}

fn replay_sample_contract_violations(
    cluster: usize,
    pair: usize,
    variant: &str,
    sample: &Sample,
    violations: &mut Vec<String>,
) {
    let closure_complete = sample.tool_closure.as_ref().is_some_and(|closure| {
        closure.complete
            && tool_closure_matches_sample(sample, closure)
            && closure.unresolved_calls.is_empty()
            && closure.orphan_calls.is_empty()
    });
    if !sample.timing_profile_valid
        || !sample.classification_complete
        || !sample.lifecycle_complete
        || !sample.token_coverage_complete
        || !sample.decision_coverage_complete
        || sample.timing_overflow_count != 0
        || sample.timing_anomaly_count != 0
        || sample.unclassified_ns != 0
        || sample.unresolved_tool_calls != 0
        || sample.orphan_tool_calls != 0
        || sample.incomplete_lifecycle_calls != 0
        || sample.unexpected_live_processes != 0
        || !closure_complete
        || !tool_graph_matches_workload(sample, AbWorkload::SessionReplay)
    {
        violations.push(format!(
            "cluster:{cluster}:pair:{pair}:{variant}:coverage_or_closure"
        ));
    }
    if sample.workload_subturns != 3 || sample.replay_subturns.len() != 3 {
        violations.push(format!(
            "cluster:{cluster}:pair:{pair}:{variant}:subturn_shape"
        ));
    } else {
        let expected = [
            (
                "actionable_success",
                "turn_complete",
                Some("passed"),
                "passed",
                0,
                true,
            ),
            (
                "required_terminal_failure",
                "turn_complete",
                Some("partial"),
                "failed",
                1,
                false,
            ),
            (
                "retained_process_abort",
                "turn_aborted",
                None,
                "canceled",
                0,
                false,
            ),
        ];
        let expected_generations = if variant == "B" {
            [4, 1, 3]
        } else {
            [10, 4, 4]
        };
        for (
            (subturn, (name, terminal, completion, result, errors, final_response)),
            expected_generations,
        ) in sample
            .replay_subturns
            .iter()
            .zip(expected)
            .zip(expected_generations)
        {
            if subturn.name != name
                || subturn.logical_generations != expected_generations
                || subturn.terminal_event != terminal
                || subturn.completion_status.as_deref() != completion
                || subturn.application_result != result
                || subturn.typed_error_count != errors
                || subturn.final_response_present != final_response
                || !subturn.closure_complete
            {
                violations.push(format!(
                    "cluster:{cluster}:pair:{pair}:{variant}:subturn_contract:{name}"
                ));
            }
        }
    }
    if variant == "B"
        && ["reviewer", "proof"].iter().any(|purpose| {
            sample
                .generation_purposes
                .get(*purpose)
                .copied()
                .unwrap_or(0)
                != 0
        })
    {
        violations.push(format!(
            "cluster:{cluster}:pair:{pair}:{variant}:reviewer_or_proof_generation"
        ));
    }
    if sample.replay_reset.as_ref().is_none_or(|reset| {
        !reset.passed || reset.before_sha256.is_empty() || reset.before_sha256 != reset.after_sha256
    }) {
        violations.push(format!(
            "cluster:{cluster}:pair:{pair}:{variant}:state_reset"
        ));
    }
    if sample.retained_write_stdin_poll_count != 2
        || sample.abort_model_resumed_call_count != 0
        || !sample.retained_process_owned_before_abort
        || sample.retained_process_count_before_abort != 1
        || sample
            .retained_abort_process_id
            .as_deref()
            .is_none_or(str::is_empty)
        || !sample.retained_process_cleanup_complete
        || !sample.retained_abort_cancellation_observed
        || sample.abort_registered_call_ids.len() < 2
        || sample.forged_turn_complete_observed
    {
        violations.push(format!(
            "cluster:{cluster}:pair:{pair}:{variant}:retained_abort_contract"
        ));
    }
}

fn replay_descriptive_latency_gate(
    clusters: &[AbPairedCluster],
    metric: AbLatencyMetric,
    pairs_per_cluster: usize,
) -> Result<AbLatencyGate> {
    let cluster = clusters
        .first()
        .context("replay latency summary requires its cluster")?;
    let a = cluster
        .a_samples
        .iter()
        .take(pairs_per_cluster)
        .map(|sample| metric.value(sample) as f64)
        .collect::<Vec<_>>();
    let b = cluster
        .b_samples
        .iter()
        .take(pairs_per_cluster)
        .map(|sample| metric.value(sample) as f64)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !a.is_empty() && a.len() == b.len(),
        "{} has incomplete replay pairs",
        metric.name()
    );
    anyhow::ensure!(
        a.iter().all(|value| *value > 0.0),
        "{} has zero A duration",
        metric.name()
    );
    let point_median_ratio = percentile(&b, 0.5) / percentile(&a, 0.5);
    let point_p95_ratio = percentile(&b, 0.95) / percentile(&a, 0.95);
    let passed = a.iter().zip(&b).all(|(a, b)| *b <= *a * AB_RATIO_TARGET);
    Ok(AbLatencyGate {
        metric: metric.name().to_string(),
        point_median_ratio,
        point_p95_ratio,
        median_ratio_lcb: point_median_ratio,
        p95_ratio_lcb: point_p95_ratio,
        median_ratio_ucb: point_median_ratio,
        p95_ratio_ucb: point_p95_ratio,
        lcb_quantile: 0.0,
        ucb_quantile: 1.0,
        pairs_per_cluster,
        median_ratio_ucb_limit: AB_RATIO_TARGET,
        p95_ratio_ucb_limit: AB_RATIO_TARGET,
        target_ratio: AB_RATIO_TARGET,
        passed,
    })
}

#[cfg(test)]
#[allow(dead_code)] // Cargo checks the no-harness benchmark with cfg(test).
fn hierarchical_paired_bootstrap(
    clusters: &[AbPairedCluster],
    metric: AbLatencyMetric,
) -> Result<AbLatencyGate> {
    hierarchical_paired_bootstrap_for_shape(
        clusters,
        metric,
        AB_CLUSTERS,
        AB_ITERATIONS,
        AbExecutionProfile::Final.config().lcb_quantile(),
        AbExecutionProfile::Final.config().ucb_quantile(),
    )
}

#[cfg(test)]
#[allow(dead_code)] // Cargo checks the no-harness benchmark with cfg(test).
fn ab_correctness_violations(
    clusters: &[AbPairedCluster],
    workload_class: AbWorkloadClass,
    workload: AbWorkload,
) -> Vec<String> {
    ab_correctness_violations_for_shape(
        clusters,
        workload_class,
        workload,
        AB_CLUSTERS,
        AB_ITERATIONS,
    )
}

#[cfg(test)]
#[allow(dead_code)] // Cargo checks the no-harness benchmark with cfg(test).
fn evaluate_ab_workload(
    clusters: &[AbPairedCluster],
    workload_class: AbWorkloadClass,
    workload: AbWorkload,
) -> Result<AbWorkloadVerdict> {
    evaluate_ab_workload_with_config(
        clusters,
        workload_class,
        workload,
        AbExecutionProfile::Final.config(),
        AB_ITERATIONS,
    )
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("open hash input {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn command_text(mut command: Command, description: &str) -> Result<String> {
    let output = command
        .output()
        .with_context(|| format!("start {description}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{description} failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn git_text(repo: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.current_dir(repo).args(args);
    command_text(command, &format!("git {}", args.join(" ")))
}

fn git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .with_context(|| format!("start git {}", args.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed with {}: {}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn git_porcelain_status(repo: &Path) -> Result<String> {
    let bytes = git_bytes(repo, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    Ok(String::from_utf8(bytes)?
        .trim_end_matches(&['\r', '\n'][..])
        .to_string())
}

const AB_OVERLAY_REPOSITORY_PATHS: [&[u8]; 5] = [
    AB_OVERLAY_REPOSITORY_PATH,
    b"codex-rs/core/benches/turn_latency/ab_contract.rs",
    b"codex-rs/core/benches/turn_latency/ab_runner.rs",
    b"codex-rs/core/benches/turn_latency/runtime_fixtures.rs",
    b"codex-rs/core/benches/turn_latency/tests.rs",
];

fn ab_overlay_path_is_owned(path: &[u8]) -> bool {
    AB_OVERLAY_REPOSITORY_PATHS.contains(&path)
}

fn controller_repository_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Some(repository_root) = manifest_dir
        .parent()
        .and_then(Path::parent)
    else {
        unreachable!("Cargo manifest directory must be nested under codex-rs/core");
    };
    repository_root.to_path_buf()
}

fn ab_overlay_sha256_at_repository(repository: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"kd4.turn_latency.overlay_closure\0");
    for path in AB_OVERLAY_REPOSITORY_PATHS {
        let Ok(path) = std::str::from_utf8(path) else {
            unreachable!("benchmark overlay repository path is UTF-8");
        };
        let bytes = fs::read(repository.join(path))
            .with_context(|| format!("read benchmark overlay {path}"))?;
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn install_ab_overlay(source_repository: &Path, destination_repository: &Path) -> Result<String> {
    let expected_sha256 = ab_overlay_sha256_at_repository(source_repository)?;
    for path in AB_OVERLAY_REPOSITORY_PATHS {
        let Ok(path) = std::str::from_utf8(path) else {
            unreachable!("benchmark overlay repository path is UTF-8");
        };
        let source = source_repository.join(path);
        let destination = destination_repository.join(path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, &destination).with_context(|| {
            format!(
                "install benchmark overlay {} into {}",
                source.display(),
                destination.display()
            )
        })?;
    }
    anyhow::ensure!(
        ab_overlay_sha256_at_repository(destination_repository)? == expected_sha256,
        "benchmark overlay identity mismatch in {}",
        destination_repository.display()
    );
    Ok(expected_sha256)
}

fn ab_overlay_status_line_is_owned(line: &str) -> bool {
    matches!(line.get(..2), Some(" M") | Some("M ") | Some("??"))
        && line
            .get(3..)
            .is_some_and(|path| ab_overlay_path_is_owned(path.as_bytes()))
}

fn canonical_filtered_tree_identity(repo: &Path, commit: &str) -> Result<String> {
    let tree = git_bytes(repo, &["ls-tree", "-r", "--full-tree", "-z", commit])?;
    anyhow::ensure!(
        tree.is_empty() || tree.ends_with(&[0]),
        "git ls-tree returned an unterminated record stream"
    );
    let mut hasher = Sha256::new();
    hasher.update(b"kd4.turn_latency.filtered_tree\0");
    hasher.update(AB_FILTERED_TREE_IDENTITY_VERSION.to_le_bytes());
    for record in tree
        .strip_suffix(&[0])
        .unwrap_or(&tree)
        .split(|byte| *byte == 0)
    {
        anyhow::ensure!(!record.is_empty(), "git ls-tree returned an empty record");
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .context("git ls-tree record omitted its path separator")?;
        if ab_overlay_path_is_owned(&record[tab + 1..]) {
            continue;
        }
        hasher.update(record);
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn clean_repo_identity(repo: &Path) -> Result<(PathBuf, String, String)> {
    let root = PathBuf::from(git_text(repo, &["rev-parse", "--show-toplevel"])?);
    let status = git_text(
        &root,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    anyhow::ensure!(status.is_empty(), "repository is dirty: {}", root.display());
    let commit = git_text(&root, &["rev-parse", "HEAD"])?;
    let tree = canonical_filtered_tree_identity(&root, &commit)?;
    Ok((root, commit, tree))
}

fn clean_main_identity(repo: &Path) -> Result<(PathBuf, String, String)> {
    let root = PathBuf::from(git_text(repo, &["rev-parse", "--show-toplevel"])?);
    let status = git_text(
        &root,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    anyhow::ensure!(status.is_empty(), "repository is dirty: {}", root.display());
    let commit = git_text(
        &root,
        &["rev-parse", "--verify", "refs/heads/main^{commit}"],
    )
    .context("resolve clean local refs/heads/main for A/B baseline")?;
    let tree = canonical_filtered_tree_identity(&root, &commit)?;
    Ok((root, commit, tree))
}

fn validate_distinct_ab_identities(
    baseline_commit: &str,
    baseline_tree: &str,
    candidate_commit: &str,
    candidate_tree: &str,
) -> Result<()> {
    anyhow::ensure!(
        baseline_commit != candidate_commit,
        "A and B resolve to the same commit"
    );
    anyhow::ensure!(
        baseline_tree != candidate_tree,
        "A and B resolve to identical filtered trees"
    );
    Ok(())
}

fn validate_squashed_candidate_parent(
    candidate_repo: &Path,
    baseline_commit: &str,
    candidate_commit: &str,
) -> Result<()> {
    let revision = git_text(
        candidate_repo,
        &["rev-list", "--parents", "-n", "1", candidate_commit],
    )?;
    let commits = revision.split_whitespace().collect::<Vec<_>>();
    anyhow::ensure!(
        commits.len() == 2 && commits[0] == candidate_commit,
        "candidate B must be a single-parent commit"
    );
    anyhow::ensure!(
        commits[1] == baseline_commit,
        "candidate B parent does not match captured baseline A"
    );
    Ok(())
}

fn a_runs_first(cluster: usize, pair_index: usize) -> bool {
    (cluster + pair_index) % 2 == 1
}

fn capture_ab_baseline(args: &AbCaptureArgs) -> Result<()> {
    let (repository, baseline_commit, baseline_filtered_tree) = clean_main_identity(&args.repo)?;
    let state = AbBaselineState {
        schema_version: AB_BASELINE_STATE_SCHEMA_VERSION,
        filtered_tree_identity_version: AB_FILTERED_TREE_IDENTITY_VERSION,
        repository,
        baseline_commit,
        baseline_filtered_tree,
    };
    if let Some(parent) = args.state.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&args.state, serde_json::to_vec_pretty(&state)?)
        .with_context(|| format!("write A/B state {}", args.state.display()))?;
    println!("{}", serde_json::to_string(&state)?);
    Ok(())
}

fn validate_ab_baseline_state(state: &AbBaselineState) -> Result<()> {
    anyhow::ensure!(
        state.schema_version == AB_BASELINE_STATE_SCHEMA_VERSION,
        "unsupported A/B state schema {}",
        state.schema_version
    );
    anyhow::ensure!(
        state.filtered_tree_identity_version == AB_FILTERED_TREE_IDENTITY_VERSION,
        "unsupported filtered-tree identity version {}",
        state.filtered_tree_identity_version
    );
    Ok(())
}

fn executable_name_for_suffix(name: &str, suffix: &str) -> String {
    format!("{name}{suffix}")
}

fn executable_name(name: &str) -> String {
    executable_name_for_suffix(name, std::env::consts::EXE_SUFFIX)
}

fn add_detached_worktree(repo: &Path, commit: &str, destination: &Path) -> Result<()> {
    anyhow::ensure!(
        !destination.exists(),
        "isolated worktree already exists: {}",
        destination.display()
    );
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new("git")
        .current_dir(repo)
        .args(["worktree", "add", "--detach"])
        .arg(destination)
        .arg(commit)
        .status()
        .with_context(|| format!("create worktree {}", destination.display()))?;
    anyhow::ensure!(status.success(), "git worktree add failed for {commit}");
    Ok(())
}

fn canonical_git_common_dir(repo: &Path) -> Result<PathBuf> {
    let common = PathBuf::from(git_text(repo, &["rev-parse", "--git-common-dir"])?);
    let common = if common.is_absolute() {
        common
    } else {
        repo.join(common)
    };
    fs::canonicalize(&common)
        .with_context(|| format!("canonicalize git common directory {}", common.display()))
}

fn reuse_detached_worktree(repo: &Path, commit: &str, destination: &Path) -> Result<()> {
    anyhow::ensure!(
        destination.is_dir(),
        "reused isolated worktree is missing: {}",
        destination.display()
    );
    let destination = fs::canonicalize(destination)?;
    let destination_root =
        fs::canonicalize(git_text(&destination, &["rev-parse", "--show-toplevel"])?)?;
    anyhow::ensure!(
        destination_root == destination,
        "reused path is not an isolated worktree root: {}",
        destination.display()
    );
    anyhow::ensure!(
        canonical_git_common_dir(repo)? == canonical_git_common_dir(&destination)?,
        "reused worktree belongs to a different repository: {}",
        destination.display()
    );
    let status = git_porcelain_status(&destination)?;
    anyhow::ensure!(
        status.lines().all(ab_overlay_status_line_is_owned),
        "reused worktree has changes outside the benchmark overlay: {}",
        destination.display()
    );
    for line in status.lines() {
        let Some(path) = line.get(3..) else {
            unreachable!("validated benchmark overlay status must carry a path");
        };
        if line.starts_with("??") {
            fs::remove_file(destination.join(path))?;
        } else {
            git_text(&destination, &["restore", "--worktree", "--", path])?;
        }
    }
    git_text(&destination, &["checkout", "--detach", commit])?;
    anyhow::ensure!(
        git_text(&destination, &["rev-parse", "HEAD"])? == commit,
        "reused worktree did not resolve requested commit {commit}"
    );
    Ok(())
}

fn cargo_target_dir_for_command(target_dir: &Path) -> PathBuf {
    dunce::simplified(target_dir).to_path_buf()
}

fn run_build_command(codex_rs: &Path, target_dir: &Path, args: &[&str]) -> Result<()> {
    #[cfg(test)]
    AB_BUILD_COMMAND_INVOCATIONS.fetch_add(1, Ordering::SeqCst);
    let command_target_dir = cargo_target_dir_for_command(target_dir);
    let status = Command::new("cargo")
        .current_dir(codex_rs)
        .env("CARGO_TARGET_DIR", &command_target_dir)
        .args(args)
        .status()
        .with_context(|| format!("run cargo {}", args.join(" ")))?;
    anyhow::ensure!(status.success(), "cargo {} failed", args.join(" "));
    Ok(())
}

#[derive(Deserialize)]
struct CargoJsonTarget {
    name: String,
    kind: Vec<String>,
}

#[derive(Deserialize)]
struct CargoJsonMessage {
    reason: String,
    #[serde(default)]
    target: Option<CargoJsonTarget>,
    #[serde(default)]
    executable: Option<PathBuf>,
}

fn select_turn_latency_executable_from_cargo_json(output: &[u8]) -> Result<PathBuf> {
    let mut executables = Vec::new();
    for (index, line) in output.split(|byte| *byte == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let message: CargoJsonMessage = serde_json::from_slice(line)
            .with_context(|| format!("parse Cargo JSON message {}", index + 1))?;
        if message.reason != "compiler-artifact" {
            continue;
        }
        let Some(target) = message.target else {
            continue;
        };
        if target.name != "turn_latency" || target.kind.len() != 1 || target.kind[0] != "bench" {
            continue;
        }
        executables.push(
            message
                .executable
                .context("turn_latency compiler artifact omitted its executable")?,
        );
    }
    anyhow::ensure!(
        executables.len() == 1,
        "expected exactly one turn_latency compiler artifact, found {}",
        executables.len()
    );
    Ok(executables.remove(0))
}

fn build_turn_latency_worker(codex_rs: &Path, target_dir: &Path) -> Result<PathBuf> {
    const ARGS: [&str; 6] = [
        "build",
        "-p",
        "codex-core",
        "--bench",
        "turn_latency",
        "--message-format=json-render-diagnostics",
    ];
    #[cfg(test)]
    AB_BUILD_COMMAND_INVOCATIONS.fetch_add(1, Ordering::SeqCst);
    let command_target_dir = cargo_target_dir_for_command(target_dir);
    let output = Command::new("cargo")
        .current_dir(codex_rs)
        .env("CARGO_TARGET_DIR", &command_target_dir)
        .args(ARGS)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("run cargo {}", ARGS.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "cargo {} failed with {}",
        ARGS.join(" "),
        output.status
    );
    select_turn_latency_executable_from_cargo_json(&output.stdout)
}

fn build_ab_variant(worktree: &Path, target_dir: &Path) -> Result<AbBuild> {
    let codex_rs = worktree.join("codex-rs");
    run_build_command(
        &codex_rs,
        target_dir,
        &["build", "-p", "codex-cli", "--bin", "codex"],
    )?;
    run_build_command(
        &codex_rs,
        target_dir,
        &[
            "build",
            "-p",
            "codex-code-mode-host",
            "--bin",
            "codex-code-mode-host",
        ],
    )?;
    let worker = build_turn_latency_worker(&codex_rs, target_dir)?;
    let cli = target_dir.join("debug").join(executable_name("codex"));
    let host = target_dir
        .join("debug")
        .join(executable_name("codex-code-mode-host"));
    anyhow::ensure!(cli.is_file(), "missing built CLI: {}", cli.display());
    anyhow::ensure!(host.is_file(), "missing built host: {}", host.display());
    anyhow::ensure!(
        worker.is_file(),
        "missing built worker: {}",
        worker.display()
    );
    Ok(AbBuild {
        worktree: worktree.to_path_buf(),
        cli,
        host,
        worker,
    })
}

fn ab_build_configuration_hash(rustc_version: &str, rust_target: &str) -> String {
    let payload = serde_json::json!({
        "profile": "dev",
        "features": Vec::<String>::new(),
        "rustc_version": rustc_version,
        "rust_target": rust_target,
        "commands": [
            ["build", "-p", "codex-cli", "--bin", "codex"],
            ["build", "-p", "codex-code-mode-host", "--bin", "codex-code-mode-host"],
            [
                "build",
                "-p",
                "codex-core",
                "--bench",
                "turn_latency",
                "--message-format=json-render-diagnostics",
            ],
        ],
        "worker_stack_bytes": AB_WORKER_STACK_BYTES,
        "workload_schema_version": AB_WORKLOAD_SCHEMA_VERSION,
        "metric_gate_version": AB_METRIC_GATE_VERSION,
        "report_schema_version": AB_REPORT_SCHEMA_VERSION,
    });
    sha256_bytes(
        &serde_json::to_vec(&payload)
            .unwrap_or_else(|error| panic!("A/B build configuration must serialize: {error}")),
    )
}

fn prepared_build(build: &AbBuild, target_dir: &Path) -> Result<AbPreparedBuild> {
    Ok(AbPreparedBuild {
        worktree: fs::canonicalize(&build.worktree)?,
        target_dir: fs::canonicalize(target_dir)?,
        cli: fs::canonicalize(&build.cli)?,
        host: fs::canonicalize(&build.host)?,
        worker: fs::canonicalize(&build.worker)?,
        cli_sha256: sha256_file(&build.cli)?,
        host_sha256: sha256_file(&build.host)?,
        worker_sha256: sha256_file(&build.worker)?,
    })
}

fn prepared_manifest_payload_hash(manifest: &AbPreparedManifest) -> Result<String> {
    let mut payload = manifest.clone();
    payload.manifest_payload_sha256.clear();
    Ok(sha256_bytes(&serde_json::to_vec(&payload)?))
}

fn validate_prepared_worktree(
    build: &AbPreparedBuild,
    commit: &str,
    filtered_tree: &str,
    overlay_sha256: &str,
) -> Result<()> {
    anyhow::ensure!(build.worktree.is_dir(), "prepared worktree is missing");
    anyhow::ensure!(
        git_text(&build.worktree, &["rev-parse", "HEAD"])? == commit,
        "prepared worktree commit no longer matches its manifest"
    );
    anyhow::ensure!(
        canonical_filtered_tree_identity(&build.worktree, commit)? == filtered_tree,
        "prepared worktree filtered tree no longer matches its manifest"
    );
    let status = String::from_utf8(git_bytes(
        &build.worktree,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?)?;
    for line in status.lines() {
        anyhow::ensure!(
            ab_overlay_status_line_is_owned(line),
            "prepared worktree has an unexpected change: {line}"
        );
    }
    anyhow::ensure!(
        ab_overlay_sha256_at_repository(&build.worktree)? == overlay_sha256,
        "prepared worktree overlay hash no longer matches its manifest"
    );
    Ok(())
}

fn validate_prepared_binary(path: &Path, expected_sha256: &str, label: &str) -> Result<()> {
    anyhow::ensure!(
        path.is_file(),
        "prepared {label} is missing: {}",
        path.display()
    );
    anyhow::ensure!(
        sha256_file(path)? == expected_sha256,
        "prepared {label} hash no longer matches its manifest"
    );
    Ok(())
}

fn validate_canonical_prepared_path(path: &Path, label: &str) -> Result<()> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalize prepared {label}: {}", path.display()))?;
    anyhow::ensure!(
        canonical == path,
        "prepared {label} path is not canonical: {}",
        path.display()
    );
    Ok(())
}

fn validate_disjoint_prepared_roots(
    left: &Path,
    left_label: &str,
    right: &Path,
    right_label: &str,
) -> Result<()> {
    anyhow::ensure!(
        left != right && !left.starts_with(right) && !right.starts_with(left),
        "prepared {left_label} and {right_label} must be isolated"
    );
    Ok(())
}

fn resolve_ab_prepare_target_dirs(args: &AbPrepareArgs) -> Result<(PathBuf, PathBuf)> {
    let (baseline, candidate) = match (
        args.baseline_target_dir.as_ref(),
        args.candidate_target_dir.as_ref(),
    ) {
        (Some(baseline), Some(candidate)) => (baseline.clone(), candidate.clone()),
        (None, None) => (
            args.work_root.join("A-target"),
            args.work_root.join("B-target"),
        ),
        _ => anyhow::bail!(
            "ab-prepare requires --baseline-target-dir and --candidate-target-dir together"
        ),
    };
    for (label, path) in [("A target", &baseline), ("B target", &candidate)] {
        fs::create_dir_all(path)
            .with_context(|| format!("create {label} cache directory: {}", path.display()))?;
    }
    Ok((
        fs::canonicalize(&baseline)
            .with_context(|| format!("canonicalize A target cache: {}", baseline.display()))?,
        fs::canonicalize(&candidate)
            .with_context(|| format!("canonicalize B target cache: {}", candidate.display()))?,
    ))
}

fn validate_ab_prepare_target_layout(
    baseline_target: &Path,
    candidate_target: &Path,
    baseline_repository: &Path,
    candidate_repository: &Path,
    baseline_worktree: &Path,
    candidate_worktree: &Path,
) -> Result<()> {
    for (label, path) in [
        ("A target", baseline_target),
        ("B target", candidate_target),
        ("A repository", baseline_repository),
        ("B repository", candidate_repository),
        ("A worktree", baseline_worktree),
        ("B worktree", candidate_worktree),
    ] {
        validate_canonical_prepared_path(path, label)?;
    }
    validate_disjoint_prepared_roots(baseline_target, "A target", candidate_target, "B target")?;
    for (target, target_label) in [
        (baseline_target, "A target"),
        (candidate_target, "B target"),
    ] {
        for (root, root_label) in [
            (baseline_repository, "A repository"),
            (candidate_repository, "B repository"),
            (baseline_worktree, "A worktree"),
            (candidate_worktree, "B worktree"),
        ] {
            validate_disjoint_prepared_roots(target, target_label, root, root_label)?;
        }
    }
    Ok(())
}

fn validate_prepared_build_layout(label: &str, build: &AbPreparedBuild) -> Result<()> {
    for (path_label, path) in [
        ("worktree", build.worktree.as_path()),
        ("target", build.target_dir.as_path()),
        ("CLI", build.cli.as_path()),
        ("host", build.host.as_path()),
        ("worker", build.worker.as_path()),
    ] {
        validate_canonical_prepared_path(path, &format!("{label} {path_label}"))?;
    }
    validate_disjoint_prepared_roots(
        &build.worktree,
        &format!("{label} worktree"),
        &build.target_dir,
        &format!("{label} target"),
    )?;
    for (binary_label, binary) in [
        ("CLI", &build.cli),
        ("host", &build.host),
        ("worker", &build.worker),
    ] {
        anyhow::ensure!(
            binary.starts_with(&build.target_dir),
            "prepared {label} {binary_label} must reside under its declared target"
        );
    }
    let binary_paths = [&build.cli, &build.host, &build.worker]
        .into_iter()
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        binary_paths.len() == 3,
        "prepared {label} binaries must use distinct paths"
    );
    Ok(())
}

fn validate_isolated_prepared_builds(manifest: &AbPreparedManifest) -> Result<()> {
    validate_prepared_build_layout("A", &manifest.baseline)?;
    validate_prepared_build_layout("B", &manifest.candidate)?;
    for (left, left_label, right, right_label) in [
        (
            manifest.baseline.worktree.as_path(),
            "A worktree",
            manifest.candidate.worktree.as_path(),
            "B worktree",
        ),
        (
            manifest.baseline.target_dir.as_path(),
            "A target",
            manifest.candidate.target_dir.as_path(),
            "B target",
        ),
        (
            manifest.baseline.worktree.as_path(),
            "A worktree",
            manifest.candidate.target_dir.as_path(),
            "B target",
        ),
        (
            manifest.candidate.worktree.as_path(),
            "B worktree",
            manifest.baseline.target_dir.as_path(),
            "A target",
        ),
    ] {
        validate_disjoint_prepared_roots(left, left_label, right, right_label)?;
    }
    let all_paths = [
        &manifest.baseline.worktree,
        &manifest.baseline.target_dir,
        &manifest.baseline.cli,
        &manifest.baseline.host,
        &manifest.baseline.worker,
        &manifest.candidate.worktree,
        &manifest.candidate.target_dir,
        &manifest.candidate.cli,
        &manifest.candidate.host,
        &manifest.candidate.worker,
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        all_paths.len() == 10,
        "prepared A/B artifacts must use pairwise-distinct paths"
    );
    Ok(())
}

fn validate_ab_prepared_manifest_contract(manifest: &AbPreparedManifest) -> Result<()> {
    anyhow::ensure!(
        manifest.schema_version == AB_PREPARED_MANIFEST_SCHEMA_VERSION,
        "unsupported A/B prepared-manifest schema {}",
        manifest.schema_version
    );
    anyhow::ensure!(
        prepared_manifest_payload_hash(manifest)? == manifest.manifest_payload_sha256,
        "A/B prepared-manifest payload hash mismatch"
    );
    validate_distinct_ab_identities(
        &manifest.baseline_commit,
        &manifest.baseline_filtered_tree,
        &manifest.candidate_commit,
        &manifest.candidate_filtered_tree,
    )?;
    anyhow::ensure!(
        ab_overlay_sha256_at_repository(&controller_repository_root())? == manifest.overlay_sha256,
        "prepared manifest was built with a different benchmark overlay"
    );
    let matrix = ab_all_workloads();
    anyhow::ensure!(
        ab_matrix_hash(matrix, ab_fixture_hash) == manifest.fixture_matrix_sha256,
        "prepared manifest fixture matrix no longer matches the controller"
    );
    anyhow::ensure!(
        ab_matrix_hash(matrix, ab_workload_schema_hash) == manifest.workload_schema_matrix_sha256,
        "prepared manifest workload schema no longer matches the controller"
    );
    // Compare executes the already-built artifacts, so the active toolchain is not
    // an input. Bind the build configuration to the toolchain recorded at prepare.
    anyhow::ensure!(
        ab_build_configuration_hash(&manifest.rustc_version, &manifest.rust_target)
            == manifest.build_configuration_sha256,
        "prepared manifest build configuration no longer matches the controller"
    );
    Ok(())
}

fn validate_ab_prepared_manifest(manifest: &AbPreparedManifest) -> Result<()> {
    validate_ab_prepared_manifest_contract(manifest)?;
    validate_isolated_prepared_builds(manifest)?;
    for (label, build, commit, filtered_tree) in [
        (
            "A",
            &manifest.baseline,
            manifest.baseline_commit.as_str(),
            manifest.baseline_filtered_tree.as_str(),
        ),
        (
            "B",
            &manifest.candidate,
            manifest.candidate_commit.as_str(),
            manifest.candidate_filtered_tree.as_str(),
        ),
    ] {
        validate_prepared_worktree(build, commit, filtered_tree, &manifest.overlay_sha256)
            .with_context(|| format!("validate prepared {label} worktree"))?;
        anyhow::ensure!(
            build.target_dir.is_dir(),
            "prepared {label} target is missing"
        );
        validate_prepared_binary(&build.cli, &build.cli_sha256, &format!("{label} CLI"))?;
        validate_prepared_binary(&build.host, &build.host_sha256, &format!("{label} host"))?;
        validate_prepared_binary(
            &build.worker,
            &build.worker_sha256,
            &format!("{label} worker"),
        )?;
    }
    Ok(())
}

fn load_ab_prepared_manifest(path: &Path) -> Result<AbPreparedManifest> {
    let manifest: AbPreparedManifest = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("read A/B manifest {}", path.display()))?,
    )?;
    validate_ab_prepared_manifest(&manifest)?;
    Ok(manifest)
}

fn build_from_prepared(prepared: &AbPreparedBuild) -> AbBuild {
    AbBuild {
        worktree: prepared.worktree.clone(),
        cli: prepared.cli.clone(),
        host: prepared.host.clone(),
        worker: prepared.worker.clone(),
    }
}

fn resolve_ab_compare_inputs(path: &Path) -> Result<(AbPreparedManifest, AbBuild, AbBuild)> {
    let manifest = load_ab_prepared_manifest(path)?;
    let baseline = build_from_prepared(&manifest.baseline);
    let candidate = build_from_prepared(&manifest.candidate);
    Ok((manifest, baseline, candidate))
}

fn write_new_ab_prepared_manifest(path: &Path, manifest: &AbPreparedManifest) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let serialized = serde_json::to_vec_pretty(manifest)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create adjacent A/B manifest for {}", path.display()))?;
    file.write_all(&serialized)?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically install new A/B manifest {}", path.display()))?;
    Ok(())
}

fn worker_stdout_receiver(stdout: ChildStdout) -> Receiver<std::result::Result<String, String>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let line = line.map_err(|error| error.to_string());
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

fn read_worker_json_line<T: for<'de> Deserialize<'de>>(
    receiver: &Receiver<std::result::Result<String, String>>,
    description: &str,
    deadline: Instant,
) -> Result<Option<T>> {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Ok(None);
    };
    match receiver.recv_timeout(remaining) {
        Ok(Ok(line)) => serde_json::from_str(line.trim_end())
            .with_context(|| format!("decode {description}: {line}"))
            .map(Some),
        Ok(Err(error)) => anyhow::bail!("read {description}: {error}"),
        Err(RecvTimeoutError::Timeout) => Ok(None),
        Err(RecvTimeoutError::Disconnected) => {
            anyhow::bail!("worker closed stdout while reading {description}")
        }
    }
}

fn terminate_ab_worker(worker: &mut AbWorkerProcess) -> Result<()> {
    if worker.child.try_wait()?.is_none() {
        worker.child.kill()?;
        let _ = worker.child.wait()?;
    }
    Ok(())
}

fn spawn_ab_worker(
    build: &AbBuild,
    variant: &str,
    cluster: usize,
    workload: AbWorkload,
    config: AbExecutionConfig,
    deadline: Instant,
) -> Result<Option<(AbWorkerProcess, AbWorkerReady)>> {
    let host_arg = build.host.to_string_lossy().into_owned();
    let cluster_arg = cluster.to_string();
    let workload_arg = workload.name();
    let warmups_arg = config.warmups.to_string();
    let samples_arg = config.max_pairs_per_cluster().to_string();
    let mut child = Command::new(&build.worker)
        .current_dir(build.worktree.join("codex-rs"))
        .env("CARGO_BIN_EXE_codex", &build.cli)
        .env("RUST_MIN_STACK", AB_WORKER_STACK_BYTES)
        .args([
            "ab-worker",
            "--code-mode-host",
            &host_arg,
            "--variant",
            variant,
            "--cluster",
            &cluster_arg,
            "--workload",
            workload_arg,
            "--warmups",
            &warmups_arg,
            "--samples",
            &samples_arg,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn {variant} worker for cluster {cluster}"))?;
    let stdin = child.stdin.take().context("worker stdin was not piped")?;
    let stdout = child.stdout.take().context("worker stdout was not piped")?;
    let mut process = AbWorkerProcess {
        child,
        stdin,
        stdout: worker_stdout_receiver(stdout),
    };
    let Some(ready): Option<AbWorkerReady> =
        read_worker_json_line(&process.stdout, "worker readiness", deadline)?
    else {
        terminate_ab_worker(&mut process)?;
        return Ok(None);
    };
    anyhow::ensure!(
        ready.kind == "ready" && ready.variant == variant && ready.cluster == cluster,
        "worker readiness identity mismatch"
    );
    anyhow::ensure!(
        ready.workload == workload,
        "worker workload routing mismatch: expected {}, got {}",
        workload.name(),
        ready.workload.name()
    );
    anyhow::ensure!(
        ready.warmups == config.warmups && ready.samples == config.max_pairs_per_cluster(),
        "worker warmup configuration mismatch"
    );
    anyhow::ensure!(
        ready.warmup_failures == ready.warmup_failure_details.len(),
        "worker warmup failure count does not match its failure details"
    );
    Ok(Some((process, ready)))
}

fn worker_sample(
    worker: &mut AbWorkerProcess,
    pair_index: usize,
    deadline: Instant,
) -> Result<Option<Sample>> {
    let command = AbWorkerCommand {
        kind: "sample".to_string(),
        pair_index: Some(pair_index),
    };
    serde_json::to_writer(&mut worker.stdin, &command)?;
    worker.stdin.write_all(b"\n")?;
    worker.stdin.flush()?;
    let Some(response): Option<AbWorkerResponse> =
        read_worker_json_line(&worker.stdout, "worker sample", deadline)?
    else {
        return Ok(None);
    };
    anyhow::ensure!(
        response.kind == "sample" && response.pair_index == pair_index,
        "worker sample identity mismatch"
    );
    Ok(Some(response.sample))
}

fn stop_ab_worker(mut worker: AbWorkerProcess, deadline: Instant) -> Result<bool> {
    if Instant::now() >= deadline {
        terminate_ab_worker(&mut worker)?;
        return Ok(false);
    }
    serde_json::to_writer(
        &mut worker.stdin,
        &AbWorkerCommand {
            kind: "stop".to_string(),
            pair_index: None,
        },
    )?;
    worker.stdin.write_all(b"\n")?;
    worker.stdin.flush()?;
    loop {
        if let Some(status) = worker.child.try_wait()? {
            anyhow::ensure!(status.success(), "A/B worker exited with {status}");
            return Ok(true);
        }
        if Instant::now() >= deadline {
            terminate_ab_worker(&mut worker)?;
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct AbLiveCluster {
    a_worker: AbWorkerProcess,
    b_worker: AbWorkerProcess,
    samples: AbPairedCluster,
}

struct AbCapturedWorkload {
    clusters: Vec<AbPairedCluster>,
    sequential_looks: Vec<AbSequentialLook>,
    cap_expired: bool,
    stopped_at_pairs_per_cluster: usize,
}

fn ab_cluster_prefixes(
    clusters: &[AbPairedCluster],
    pairs_per_cluster: usize,
) -> Result<Vec<AbPairedCluster>> {
    clusters
        .iter()
        .map(|cluster| {
            anyhow::ensure!(
                cluster.a_first.len() >= pairs_per_cluster
                    && cluster.a_samples.len() >= pairs_per_cluster
                    && cluster.b_samples.len() >= pairs_per_cluster,
                "cluster {} cannot provide a {pairs_per_cluster}-pair prefix",
                cluster.cluster
            );
            let mut prefix = cluster.clone();
            prefix.a_first.truncate(pairs_per_cluster);
            prefix.a_samples.truncate(pairs_per_cluster);
            prefix.b_samples.truncate(pairs_per_cluster);
            Ok(prefix)
        })
        .collect()
}

fn start_ab_cluster(
    a: &AbBuild,
    b: &AbBuild,
    cluster: usize,
    workload: AbWorkload,
    config: AbExecutionConfig,
    deadline: Instant,
) -> Result<Option<AbLiveCluster>> {
    let Some((mut a_worker, a_ready)) =
        spawn_ab_worker(a, "A", cluster, workload, config, deadline)?
    else {
        return Ok(None);
    };
    let Some((b_worker, b_ready)) = spawn_ab_worker(b, "B", cluster, workload, config, deadline)?
    else {
        terminate_ab_worker(&mut a_worker)?;
        return Ok(None);
    };
    Ok(Some(AbLiveCluster {
        a_worker,
        b_worker,
        samples: AbPairedCluster {
            cluster,
            a_first: Vec::with_capacity(config.max_pairs_per_cluster()),
            a_samples: Vec::with_capacity(config.max_pairs_per_cluster()),
            b_samples: Vec::with_capacity(config.max_pairs_per_cluster()),
            a_warmup_failures: a_ready.warmup_failures,
            b_warmup_failures: b_ready.warmup_failures,
            a_warmup_failure_details: a_ready.warmup_failure_details,
            b_warmup_failure_details: b_ready.warmup_failure_details,
        },
    }))
}

fn extend_ab_cluster(
    live: &mut AbLiveCluster,
    target_pairs: usize,
    deadline: Instant,
) -> Result<bool> {
    while live.samples.a_first.len() < target_pairs {
        let pair_index = live.samples.a_first.len();
        let cluster = live.samples.cluster;
        let run_a_first = a_runs_first(cluster, pair_index);
        live.samples.a_first.push(run_a_first);
        if run_a_first {
            let Some(sample) = worker_sample(&mut live.a_worker, pair_index, deadline)? else {
                return Ok(false);
            };
            live.samples.a_samples.push(sample);
            let Some(sample) = worker_sample(&mut live.b_worker, pair_index, deadline)? else {
                return Ok(false);
            };
            live.samples.b_samples.push(sample);
        } else {
            let Some(sample) = worker_sample(&mut live.b_worker, pair_index, deadline)? else {
                return Ok(false);
            };
            live.samples.b_samples.push(sample);
            let Some(sample) = worker_sample(&mut live.a_worker, pair_index, deadline)? else {
                return Ok(false);
            };
            live.samples.a_samples.push(sample);
        }
    }
    Ok(true)
}

fn close_ab_clusters(
    live_clusters: Vec<AbLiveCluster>,
    deadline: Instant,
    force: bool,
) -> Result<(Vec<AbPairedCluster>, bool)> {
    let mut cap_expired = force;
    let mut clusters = Vec::with_capacity(live_clusters.len());
    for live in live_clusters {
        let AbLiveCluster {
            mut a_worker,
            mut b_worker,
            samples,
        } = live;
        if force {
            terminate_ab_worker(&mut a_worker)?;
            terminate_ab_worker(&mut b_worker)?;
        } else if !stop_ab_worker(a_worker, deadline)? {
            cap_expired = true;
            terminate_ab_worker(&mut b_worker)?;
        } else if !stop_ab_worker(b_worker, deadline)? {
            cap_expired = true;
        }
        clusters.push(samples);
    }
    Ok((clusters, cap_expired))
}

fn capture_ab_workload(
    a: &AbBuild,
    b: &AbBuild,
    workload: AbWorkload,
    config: AbExecutionConfig,
    deadline: Instant,
) -> Result<AbCapturedWorkload> {
    let mut live_clusters = Vec::with_capacity(config.clusters);
    for cluster in 1..=config.clusters {
        let Some(live) = start_ab_cluster(a, b, cluster, workload, config, deadline)? else {
            let (clusters, _) = close_ab_clusters(live_clusters, deadline, true)?;
            return Ok(AbCapturedWorkload {
                clusters,
                sequential_looks: Vec::new(),
                cap_expired: true,
                stopped_at_pairs_per_cluster: 0,
            });
        };
        live_clusters.push(live);
    }

    let workload_looks = config.looks_for(workload);
    let mut sequential_looks = Vec::with_capacity(workload_looks.len());
    let mut cap_expired = false;
    let mut stopped_at_pairs_per_cluster = 0;
    for pairs_per_cluster in workload_looks.iter().copied() {
        for live in &mut live_clusters {
            if !extend_ab_cluster(live, pairs_per_cluster, deadline)? {
                cap_expired = true;
                break;
            }
        }
        if cap_expired {
            break;
        }
        stopped_at_pairs_per_cluster = pairs_per_cluster;
        let observed_clusters = live_clusters
            .iter()
            .map(|live| live.samples.clone())
            .collect::<Vec<_>>();
        let clusters = ab_cluster_prefixes(&observed_clusters, pairs_per_cluster)?;
        let verdict = evaluate_ab_workload_with_config(
            &clusters,
            workload.class(),
            workload,
            config,
            pairs_per_cluster,
        )?;
        let passed = verdict.passed;
        let decision = verdict.decision;
        sequential_looks.push(AbSequentialLook {
            pairs_per_cluster,
            total_pairs: pairs_per_cluster * config.clusters,
            ucb_quantile: config.ucb_quantile(),
            latency_gates: verdict.latency_gates,
            latency_diagnostics: verdict.latency_diagnostics,
            correctness_violations: verdict.correctness_violations,
            decision,
            stop_reason: verdict.stop_reason,
            passed,
        });
        if decision != AbSequentialDecision::Continue {
            break;
        }
    }
    let (clusters, stop_cap_expired) = close_ab_clusters(live_clusters, deadline, cap_expired)?;
    Ok(AbCapturedWorkload {
        clusters,
        sequential_looks,
        cap_expired: cap_expired || stop_cap_expired,
        stopped_at_pairs_per_cluster,
    })
}

const AB_REPLAY_ACTION_PROMPT: &str = "Inspect the exact turn-latency benchmark owner, implementation, and direct test, then perform the targeted benchmark action.";
const AB_REPLAY_ACTION_REPLY: &str = "targeted replay action complete";
const AB_REPLAY_FAILURE_PROMPT: &str = "Run the deterministic required terminal-failure action.";
const AB_REPLAY_HISTORY_SEED_PREFIX: &str = "replay-stable-history-";
const AB_REPLAY_HISTORY_SEED_REPLY: &str = "replay history recorded";
const AB_REPLAY_ACTION_FIRST_MARKER: &str =
    "first assistant response must inspect the exact source owner";
const AB_REPLAY_TARGET_PATH: &str = "codex-rs/core/benches/turn_latency.rs";
const AB_REPLAY_TEST_PATH: &str = "codex-rs/core/benches/turn_latency/tests.rs";
const AB_REPLAY_BASELINE_MARKER: &str = "__KD4_REPLAY_STATE_BASELINE__";
const AB_REPLAY_MUTATED_MARKER: &str = "__KD4_REPLAY_STATE_MUTATED__";
const AB_REPLAY_VALIDATION_MARKER: &str = "__KD4_REPLAY_VALIDATION_PASSED__";
const AB_REPLAY_VALIDATION_TEST_PATH: &str = "replay_validation_test.py";
const AB_REPLAY_VALIDATION_SELECTOR: &str = "replay_validation_test.ReplayValidation.test_mutation";
const AB_REPLAY_OWNER_PATHS: [&str; 1] = ["source_owners.toml"];
const AB_REPLAY_SOURCE_PATHS: [&str; 2] = [AB_REPLAY_TARGET_PATH, AB_REPLAY_TEST_PATH];
const AB_REPLAY_BROAD_PATHS_ONE: [&str; 1] = ["."];
const AB_REPLAY_BROAD_PATHS_TWO: [&str; 1] = ["codex-rs"];
const AB_REPLAY_ACTION_CONTENTION_SOURCE: &str = r#"
const plan = {
  plan: [{
    id: "targeted-replay-action",
    step: "mutate and validate the exact replay benchmark target",
    status: "in_progress",
    acceptance_criteria: ["the exact target is mutated and its direct contract passes"],
    runtime_paths: [
      "codex-rs/core/benches/turn_latency.rs",
      "codex-rs/core/benches/turn_latency/tests.rs",
    ],
    validation_route: {
      ordering: "stop_on_failure",
      leaves: [{
        argv: [
          "python",
          "-m",
          "unittest",
          "replay_validation_test.ReplayValidation.test_mutation",
        ],
        covered_paths: [
          "codex-rs/core/benches/turn_latency.rs",
          "codex-rs/core/benches/turn_latency/tests.rs",
        ],
        timeout_ms: 10000,
      }],
    },
  }],
};
await Promise.all([
  tools.update_plan(plan),
  tools.update_plan(plan),
  tools.update_plan(plan),
  tools.update_plan(plan),
  tools.update_plan(plan),
]);
"#;
const AB_REPLAY_BROAD_CONTENTION_SOURCE: &str = r#"
await Promise.all([
  tools.update_plan({ plan: [{ step: "broad discovery one", status: "in_progress" }] }),
  tools.update_plan({ plan: [{ step: "broad discovery two", status: "pending" }] }),
  tools.update_plan({ plan: [{ step: "broad discovery three", status: "pending" }] }),
  tools.update_plan({ plan: [{ step: "broad discovery four", status: "pending" }] }),
  tools.update_plan({ plan: [{ step: "broad discovery five", status: "pending" }] }),
]);
"#;
const AB_REPLAY_WAIT_PATCH: &str =
    "*** Begin Patch\n*** Update File: state/wait.txt\n@@\n-baseline\n+waited\n*** End Patch";
const AB_REPLAY_REPAIR_PATCH: &str =
    "*** Begin Patch\n*** Update File: state/repair.txt\n@@\n-baseline\n+repaired\n*** End Patch";
const AB_REPLAY_MUTATION_PATCH: &str = "*** Begin Patch\n*** Update File: codex-rs/core/benches/turn_latency.rs\n@@\n-__KD4_REPLAY_STATE_BASELINE__\n+__KD4_REPLAY_STATE_MUTATED__\n*** End Patch";
const AB_REPLAY_REQUIRED_FAILURE_SOURCE: &str =
    r#"throw new Error("required replay terminal failure");"#;
const AB_REPLAY_FAILURE_DIAGNOSIS_SOURCE: &str = r#"
await Promise.all([
  tools.update_plan({ plan: [{ step: "diagnose required terminal failure", status: "in_progress" }] }),
  tools.update_plan({ plan: [{ step: "wait after required terminal failure", status: "pending" }] }),
]);
"#;
const AB_REPLAY_FAILURE_REPAIR_SOURCE: &str = r#"
await tools.update_plan({ plan: [{ step: "attempt repair after required terminal failure", status: "in_progress" }] });
"#;
const AB_REPLAY_RETAINED_WAIT_SOURCE: &str = r#"
await tools.update_plan({ plan: [{ step: "avoidable retained-process wait", status: "in_progress" }] });
"#;
const AB_REPLAY_VALIDATION_TEST_SOURCE: &str = r#"import pathlib
import unittest


class ReplayValidation(unittest.TestCase):
    def test_mutation(self):
        root = pathlib.Path(__file__).resolve().parent
        source = (root / "codex-rs/core/benches/turn_latency.rs").read_text(encoding="utf-8")
        direct_test = (root / "codex-rs/core/benches/turn_latency/tests.rs").read_text(encoding="utf-8")
        self.assertIn("__KD4_REPLAY_STATE_MUTATED__", source)
        self.assertIn("__KD4_REPLAY_STATE_MUTATED__", direct_test)
        print("__KD4_REPLAY_VALIDATION_PASSED__")
"#;

fn replay_request_has_action_first_instruction(request: &wiremock::Request) -> bool {
    high_volume_request_body_json(request)
        .to_string()
        .contains(AB_REPLAY_ACTION_FIRST_MARKER)
}

struct ReplayActionResponder {
    fixture_id: String,
    fixture_program: PathBuf,
    fixture_root: PathBuf,
    response_count: AtomicUsize,
    action_response_stage: Arc<AtomicUsize>,
    failure_response_stage: Arc<AtomicUsize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplayActionResponseRoute {
    Action {
        action_first: bool,
        output_count: usize,
    },
    Failure {
        action_first: bool,
        output_count: usize,
    },
    HistorySeed,
    Unexpected,
}

fn replay_action_response_route(
    body: &serde_json::Value,
    action_first: bool,
) -> ReplayActionResponseRoute {
    let last_user = request_last_user_item(body)
        .map(serde_json::Value::to_string)
        .unwrap_or_default();
    let output_count = request_top_level_tool_output_count_after_current_input(body);
    if last_user.contains(AB_REPLAY_ACTION_PROMPT) {
        ReplayActionResponseRoute::Action {
            action_first,
            output_count,
        }
    } else if last_user.contains(AB_REPLAY_FAILURE_PROMPT) {
        ReplayActionResponseRoute::Failure {
            action_first,
            output_count,
        }
    } else if last_user.contains(AB_REPLAY_HISTORY_SEED_PREFIX) {
        ReplayActionResponseRoute::HistorySeed
    } else {
        ReplayActionResponseRoute::Unexpected
    }
}

fn replay_response_stage(
    route: ReplayActionResponseRoute,
    action_response_stage: &AtomicUsize,
    failure_response_stage: &AtomicUsize,
) -> usize {
    match route {
        ReplayActionResponseRoute::Action { .. } => {
            action_response_stage.fetch_add(1, Ordering::SeqCst)
        }
        ReplayActionResponseRoute::Failure { .. } => {
            failure_response_stage.fetch_add(1, Ordering::SeqCst)
        }
        ReplayActionResponseRoute::HistorySeed | ReplayActionResponseRoute::Unexpected => 0,
    }
}

fn reset_replay_response_stages(
    action_response_stage: &AtomicUsize,
    failure_response_stage: &AtomicUsize,
) {
    action_response_stage.store(0, Ordering::SeqCst);
    failure_response_stage.store(0, Ordering::SeqCst);
}

fn replay_exec_events(
    response_id: &str,
    call_suffix: &str,
    source: &str,
    input_tokens: u64,
) -> Vec<serde_json::Value> {
    vec![
        ev_response_created(response_id),
        ev_custom_tool_call(&format!("{response_id}-{call_suffix}"), "exec", source),
        ev_completed_with_usage(
            response_id,
            input_tokens,
            input_tokens.saturating_sub(512),
            24,
            8,
        ),
    ]
}

fn replay_exec_command_event(
    call_id: &str,
    fixture_program: &Path,
    fixture_root: &Path,
    mode: &str,
    paths: &[&str],
) -> serde_json::Value {
    let mut child_args = vec!["ab-replay-command".to_string(), mode.to_string()];
    child_args.extend(paths.iter().map(|path| (*path).to_string()));
    let arguments = serde_json::json!({
        "kind": "argv",
        "program": fixture_program,
        "args": child_args,
        "workdir": fixture_root,
        "yield_time_ms": 10_000,
        "tty": false,
    });
    ev_function_call(
        call_id,
        "exec_command",
        &serde_json::to_string(&arguments)
            .unwrap_or_else(|error| panic!("serialize replay exec_command arguments: {error}")),
    )
}

fn replay_direct_exec_command_events(
    response_id: &str,
    call_suffix: &str,
    fixture_program: &Path,
    fixture_root: &Path,
    mode: &str,
    paths: &[&str],
    input_tokens: u64,
) -> Vec<serde_json::Value> {
    vec![
        ev_response_created(response_id),
        replay_exec_command_event(
            &format!("{response_id}-{call_suffix}"),
            fixture_program,
            fixture_root,
            mode,
            paths,
        ),
        ev_completed_with_usage(
            response_id,
            input_tokens,
            input_tokens.saturating_sub(512),
            24,
            8,
        ),
    ]
}

fn replay_direct_validation_events(response_id: &str, input_tokens: u64) -> Vec<serde_json::Value> {
    let validation = serde_json::json!({
        "covered_paths": AB_REPLAY_SOURCE_PATHS,
    });
    let arguments = serde_json::json!({
        "kind": "argv",
        "program": "python",
        "args": ["-m", "unittest", AB_REPLAY_VALIDATION_SELECTOR],
        "yield_time_ms": 10_000,
        "tty": false,
        "validation": validation,
    });
    vec![
        ev_response_created(response_id),
        ev_function_call(
            &format!("{response_id}-validation"),
            "exec_command",
            &serde_json::to_string(&arguments).unwrap_or_else(|error| {
                panic!("serialize replay validation exec_command arguments: {error}")
            }),
        ),
        ev_completed_with_usage(
            response_id,
            input_tokens,
            input_tokens.saturating_sub(512),
            24,
            8,
        ),
    ]
}

fn replay_direct_patch_events(
    response_id: &str,
    call_suffix: &str,
    patch: &str,
    input_tokens: u64,
) -> Vec<serde_json::Value> {
    vec![
        ev_response_created(response_id),
        ev_apply_patch_custom_tool_call(&format!("{response_id}-{call_suffix}"), patch),
        ev_completed_with_usage(
            response_id,
            input_tokens,
            input_tokens.saturating_sub(512),
            24,
            8,
        ),
    ]
}

// The fixture mirrors two ordered command branches plus their shared source.
#[allow(clippy::too_many_arguments)]
fn replay_mixed_contention_events(
    response_id: &str,
    call_prefix: &str,
    fixture_program: &Path,
    fixture_root: &Path,
    first_mode: &str,
    first_paths: &[&str],
    second_mode: &str,
    second_paths: &[&str],
    source: &str,
) -> Vec<serde_json::Value> {
    vec![
        ev_response_created(response_id),
        replay_exec_command_event(
            &format!("{response_id}-{call_prefix}-owner"),
            fixture_program,
            fixture_root,
            first_mode,
            first_paths,
        ),
        replay_exec_command_event(
            &format!("{response_id}-{call_prefix}-source"),
            fixture_program,
            fixture_root,
            second_mode,
            second_paths,
        ),
        ev_custom_tool_call(
            &format!("{response_id}-{call_prefix}-contention"),
            "exec",
            source,
        ),
        ev_completed_with_usage(response_id, 2_048, 1_536, 48, 16),
    ]
}

impl wiremock::Respond for ReplayActionResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let request_index = self.response_count.fetch_add(1, Ordering::SeqCst);
        let response_id = format!("{}-{request_index}", self.fixture_id);
        let body = high_volume_request_body_json(request);
        let route = replay_action_response_route(
            &body,
            replay_request_has_action_first_instruction(request),
        );
        let response_stage = replay_response_stage(
            route,
            self.action_response_stage.as_ref(),
            self.failure_response_stage.as_ref(),
        );
        let events = match (route, response_stage) {
            (
                ReplayActionResponseRoute::Action {
                    action_first: true, ..
                },
                0,
            ) => replay_mixed_contention_events(
                &response_id,
                "targeted",
                &self.fixture_program,
                &self.fixture_root,
                "read",
                &AB_REPLAY_OWNER_PATHS,
                "read",
                &AB_REPLAY_SOURCE_PATHS,
                AB_REPLAY_ACTION_CONTENTION_SOURCE,
            ),
            (
                ReplayActionResponseRoute::Action {
                    action_first: true, ..
                },
                1,
            ) => replay_direct_patch_events(
                &response_id,
                "mutation",
                AB_REPLAY_MUTATION_PATCH,
                2_304,
            ),
            (
                ReplayActionResponseRoute::Action {
                    action_first: true, ..
                },
                2,
            ) => replay_direct_validation_events(&response_id, 2_304),
            (
                ReplayActionResponseRoute::Action {
                    action_first: true, ..
                },
                3,
            ) => vec![
                ev_assistant_message(&response_id, AB_REPLAY_ACTION_REPLY),
                ev_completed_with_usage(&response_id, 2_304, 1_792, 16, 0),
            ],
            (
                ReplayActionResponseRoute::Action {
                    action_first: false,
                    ..
                },
                0,
            ) => replay_mixed_contention_events(
                &response_id,
                "broad-one",
                &self.fixture_program,
                &self.fixture_root,
                "broad",
                &AB_REPLAY_BROAD_PATHS_ONE,
                "broad",
                &AB_REPLAY_BROAD_PATHS_TWO,
                AB_REPLAY_BROAD_CONTENTION_SOURCE,
            ),
            (
                ReplayActionResponseRoute::Action {
                    action_first: false,
                    ..
                },
                1,
            ) => replay_mixed_contention_events(
                &response_id,
                "broad-two",
                &self.fixture_program,
                &self.fixture_root,
                "broad",
                &AB_REPLAY_BROAD_PATHS_ONE,
                "broad",
                &AB_REPLAY_BROAD_PATHS_TWO,
                AB_REPLAY_BROAD_CONTENTION_SOURCE,
            ),
            (
                ReplayActionResponseRoute::Action {
                    action_first: false,
                    ..
                },
                2,
            ) => replay_direct_patch_events(&response_id, "wait", AB_REPLAY_WAIT_PATCH, 2_304),
            (
                ReplayActionResponseRoute::Action {
                    action_first: false,
                    ..
                },
                3,
            ) => replay_direct_patch_events(&response_id, "repair", AB_REPLAY_REPAIR_PATCH, 2_304),
            (
                ReplayActionResponseRoute::Action {
                    action_first: false,
                    ..
                },
                4,
            ) => replay_mixed_contention_events(
                &response_id,
                "targeted",
                &self.fixture_program,
                &self.fixture_root,
                "read",
                &AB_REPLAY_OWNER_PATHS,
                "read",
                &AB_REPLAY_SOURCE_PATHS,
                AB_REPLAY_ACTION_CONTENTION_SOURCE,
            ),
            (
                ReplayActionResponseRoute::Action {
                    action_first: false,
                    ..
                },
                5,
            ) => replay_direct_patch_events(
                &response_id,
                "mutation",
                AB_REPLAY_MUTATION_PATCH,
                2_304,
            ),
            (
                ReplayActionResponseRoute::Action {
                    action_first: false,
                    ..
                },
                6,
            ) => replay_direct_validation_events(&response_id, 2_304),
            (
                ReplayActionResponseRoute::Action {
                    action_first: false,
                    ..
                },
                7,
            ) => replay_direct_exec_command_events(
                &response_id,
                "evidence",
                &self.fixture_program,
                &self.fixture_root,
                "evidence",
                &[AB_REPLAY_TARGET_PATH, "state/wait.txt", "state/repair.txt"],
                2_304,
            ),
            (
                ReplayActionResponseRoute::Action {
                    action_first: false,
                    ..
                },
                8,
            ) => replay_direct_exec_command_events(
                &response_id,
                "review",
                &self.fixture_program,
                &self.fixture_root,
                "review",
                &[AB_REPLAY_TARGET_PATH],
                2_304,
            ),
            (
                ReplayActionResponseRoute::Action {
                    action_first: false,
                    ..
                },
                9,
            ) => vec![
                ev_assistant_message(&response_id, AB_REPLAY_ACTION_REPLY),
                ev_completed_with_usage(&response_id, 2_304, 1_792, 16, 0),
            ],
            (ReplayActionResponseRoute::Failure { .. }, 0) => replay_exec_events(
                &response_id,
                "required-failure",
                AB_REPLAY_REQUIRED_FAILURE_SOURCE,
                1_536,
            ),
            (
                ReplayActionResponseRoute::Failure {
                    action_first: false,
                    ..
                },
                1,
            ) => replay_exec_events(
                &response_id,
                "failure-diagnosis",
                AB_REPLAY_FAILURE_DIAGNOSIS_SOURCE,
                1_792,
            ),
            (
                ReplayActionResponseRoute::Failure {
                    action_first: false,
                    ..
                },
                2,
            ) => replay_exec_events(
                &response_id,
                "failure-repair",
                AB_REPLAY_FAILURE_REPAIR_SOURCE,
                1_792,
            ),
            (
                ReplayActionResponseRoute::Failure {
                    action_first: false,
                    ..
                },
                3,
            ) => vec![
                ev_assistant_message(&response_id, "forbidden replay failure resume"),
                ev_completed_with_usage(&response_id, 1_792, 1_280, 8, 0),
            ],
            (ReplayActionResponseRoute::HistorySeed, _) => vec![
                ev_assistant_message(&response_id, AB_REPLAY_HISTORY_SEED_REPLY),
                ev_completed_with_usage(&response_id, 1_024, 768, 8, 0),
            ],
            (ReplayActionResponseRoute::Action { .. }, _)
            | (ReplayActionResponseRoute::Failure { .. }, _)
            | (ReplayActionResponseRoute::Unexpected, _) => vec![
                ev_assistant_message(&response_id, "unexpected replay route"),
                ev_completed_with_usage(&response_id, 1_024, 768, 8, 0),
            ],
        };
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse(events))
    }
}

struct ReplayActionFixture {
    _server: wiremock::MockServer,
    test: TestCodex,
    request_capture: HighVolumeRequestCapture,
    action_response_stage: Arc<AtomicUsize>,
    failure_response_stage: Arc<AtomicUsize>,
}

fn replay_git(root: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("run replay fixture git {}", args.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "replay fixture git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn initialize_replay_workspace(root: &Path) -> Result<()> {
    let target = root.join(AB_REPLAY_TARGET_PATH);
    let direct_test = root.join(AB_REPLAY_TEST_PATH);
    let wait_state = root.join("state/wait.txt");
    let repair_state = root.join("state/repair.txt");
    for parent in [target.parent(), direct_test.parent(), wait_state.parent()]
        .into_iter()
        .flatten()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create replay fixture directory {}", parent.display()))?;
    }
    fs::write(
        root.join("source_owners.toml"),
        format!(
            "[owners.turn_latency]\npaths = [\"{AB_REPLAY_TARGET_PATH}\", \"{AB_REPLAY_TEST_PATH}\"]\n"
        ),
    )?;
    fs::write(root.join("kd4_features.toml"), "")?;
    fs::write(
        root.join(AB_REPLAY_VALIDATION_TEST_PATH),
        AB_REPLAY_VALIDATION_TEST_SOURCE,
    )?;
    fs::write(
        &target,
        format!("// deterministic session replay target\n{AB_REPLAY_BASELINE_MARKER}\n"),
    )?;
    fs::write(
        &direct_test,
        format!("// direct replay contract expects {AB_REPLAY_MUTATED_MARKER}\n"),
    )?;
    fs::write(&wait_state, "baseline\n")?;
    fs::write(&repair_state, "baseline\n")?;
    replay_git(root, &["init", "--quiet"])?;
    replay_git(root, &["config", "core.autocrlf", "false"])?;
    replay_git(root, &["config", "user.email", "replay@example.invalid"])?;
    replay_git(root, &["config", "user.name", "KD4 Replay"])?;
    replay_git(root, &["add", "--all"])?;
    replay_git(root, &["commit", "--quiet", "-m", "replay baseline"])?;
    Ok(())
}

fn replay_workspace_fingerprint(root: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    for relative in [
        "source_owners.toml",
        "kd4_features.toml",
        AB_REPLAY_VALIDATION_TEST_PATH,
        AB_REPLAY_TARGET_PATH,
        AB_REPLAY_TEST_PATH,
        "state/wait.txt",
        "state/repair.txt",
    ] {
        bytes.extend_from_slice(relative.as_bytes());
        bytes.extend_from_slice(&fs::read(root.join(relative))?);
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "--untracked-files=all"])
        .output()
        .context("capture replay fixture git status")?;
    anyhow::ensure!(status.status.success(), "capture replay fixture git status");
    bytes.extend_from_slice(&status.stdout);
    Ok(sha256_bytes(&bytes))
}

fn reset_replay_workspace(root: &Path) -> Result<()> {
    replay_git(root, &["reset", "--hard", "--quiet", "HEAD"])?;
    replay_git(root, &["clean", "-fd", "--quiet"])?;
    Ok(())
}

impl ReplayActionFixture {
    async fn start(code_mode_host: &Path, fixture_id: &str) -> Result<Self> {
        let server = start_mock_server().await;
        let request_capture = HighVolumeRequestCapture::default();
        let action_response_stage = Arc::new(AtomicUsize::new(0));
        let failure_response_stage = Arc::new(AtomicUsize::new(0));
        let fixture_program =
            std::env::current_exe().context("resolve session-replay action fixture executable")?;
        let test = test_codex()
            .with_model("test-gpt-5.1-codex")
            .with_code_mode_host_program(code_mode_host.to_path_buf())
            .with_workspace_setup(|cwd, _filesystem| async move {
                initialize_replay_workspace(cwd.as_path())
            })
            .with_config(|config| {
                let _ = config.features.enable(Feature::UnifiedExec);
                let _ = config.features.enable(Feature::CodeMode);
                let _ = config.features.disable(Feature::TaskCompletionReviewer);
            })
            .build(&server)
            .await?;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(".*/responses$"))
            .and(request_capture.clone())
            .respond_with(ReplayActionResponder {
                fixture_id: fixture_id.to_string(),
                fixture_program,
                fixture_root: test.cwd_path().to_path_buf(),
                response_count: AtomicUsize::new(0),
                action_response_stage: Arc::clone(&action_response_stage),
                failure_response_stage: Arc::clone(&failure_response_stage),
            })
            .mount(&server)
            .await;
        for seed_index in 0..AB_LONG_HISTORY_TURNS {
            let seed = format!(
                "{AB_REPLAY_HISTORY_SEED_PREFIX}{seed_index:02}:{}",
                "h".repeat(AB_LONG_HISTORY_SEED_BYTES)
            );
            let completion = test.submit_turn_and_capture_completion(&seed).await?;
            anyhow::ensure!(
                completion.error.is_none()
                    && completion.last_agent_message.as_deref()
                        == Some(AB_REPLAY_HISTORY_SEED_REPLY),
                "failed to seed session-replay long history"
            );
        }
        Ok(Self {
            _server: server,
            test,
            request_capture,
            action_response_stage,
            failure_response_stage,
        })
    }

    async fn turn(&self, prompt: &str) -> (Sample, Vec<wiremock::Request>) {
        let requests_before = self.request_capture.request_count();
        let started = Instant::now();
        let completion = self.test.submit_turn_and_capture_completion(prompt).await;
        let duration_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let requests = self.request_capture.requests_since(requests_before);
        let mut sample = match completion {
            Ok(completion) => {
                let mut sample = completion
                    .timing
                    .as_ref()
                    .map(sample_from_timing)
                    .unwrap_or_default();
                sample.duration_ns = duration_ns;
                sample.workload_subturns = 1;
                sample.terminal_event = "turn_complete".to_string();
                sample.completion_status = completion.completion.as_ref().map(|gate| {
                    match gate.status {
                        TaskCompletionStatus::Passed => "passed",
                        TaskCompletionStatus::Partial => "partial",
                        TaskCompletionStatus::Blocked => "blocked",
                    }
                    .to_string()
                });
                sample.typed_error_count = u32::from(completion.error.is_some());
                sample.final_response_present = completion.last_agent_message.is_some();
                if prompt == AB_REPLAY_ACTION_PROMPT
                    && let Some(error) = completion.error.as_ref()
                {
                    sample
                        .failure_codes
                        .push(format!("replay_action_terminal_error:{}", error.message));
                }
                sample.serialized_bytes = requests
                    .iter()
                    .map(|request| request.body.len() as u64)
                    .sum();
                sample.prompt_input_tokens = requests
                    .iter()
                    .map(high_volume_request_body_json)
                    .map(|body| prompt_input_tokens_from_body(&body))
                    .sum();
                sample.history_seed_turns_visible = requests
                    .first()
                    .map(high_volume_request_body_json)
                    .as_ref()
                    .map(|body| {
                        body.get("input")
                            .and_then(serde_json::Value::as_array)
                            .map_or(0, |input| {
                                input
                                    .iter()
                                    .filter(|item| {
                                        item.to_string().contains(AB_REPLAY_HISTORY_SEED_PREFIX)
                                    })
                                    .count()
                                    .min(u32::MAX as usize) as u32
                            })
                    })
                    .unwrap_or_default();
                sample
            }
            Err(error) => Sample {
                duration_ns,
                workload_subturns: 1,
                failed: true,
                failure_codes: vec![format!("replay_turn_completion:{error}")],
                ..Sample::default()
            },
        };
        sample.failed = !sample.failure_codes.is_empty();
        (sample, requests)
    }

    async fn action_and_failure(
        &self,
    ) -> (
        Sample,
        bool,
        AbReplaySubturnRecord,
        AbReplaySubturnRecord,
        u32,
    ) {
        reset_replay_response_stages(
            self.action_response_stage.as_ref(),
            self.failure_response_stage.as_ref(),
        );
        let (mut action, action_requests) = self.turn(AB_REPLAY_ACTION_PROMPT).await;
        let action_instruction = action_requests
            .first()
            .is_some_and(replay_request_has_action_first_instruction);
        let targeted_inspection = action_instruction
            && action_requests.get(1).is_some_and(|request| {
                let body = high_volume_request_body_json(request).to_string();
                body.contains(AB_REPLAY_TARGET_PATH)
                    && body.contains(AB_REPLAY_TEST_PATH)
                    && body.contains(AB_REPLAY_BASELINE_MARKER)
            });
        let mutation_observed = action_requests.iter().any(|request| {
            high_volume_request_body_json(request)
                .to_string()
                .contains(AB_REPLAY_MUTATED_MARKER)
        });
        let validation_observed = action_requests.iter().any(|request| {
            high_volume_request_body_json(request)
                .to_string()
                .contains(AB_REPLAY_VALIDATION_MARKER)
        });
        let targeted_action = targeted_inspection && mutation_observed && validation_observed;
        let actionable_success = mutation_observed
            && validation_observed
            && action.typed_error_count == 0
            && action.completion_status.as_deref() == Some("passed")
            && action.final_response_present
            && action.lifecycle_complete;
        let action_target = if action_instruction { 4 } else { 10 };
        let (expected_direct, expected_nested) = if action_instruction { (5, 5) } else { (15, 15) };
        if action.direct_tool_calls != expected_direct
            || action.nested_tool_calls != expected_nested
        {
            action
                .failure_codes
                .push("replay_action_tool_graph".to_string());
        }
        if action_instruction && !targeted_action {
            action
                .failure_codes
                .push("replay_targeted_action_not_observed".to_string());
        }
        if action_instruction && action.completion_status.as_deref() != Some("passed") {
            action
                .failure_codes
                .push("replay_action_protocol_not_passed".to_string());
        }
        let action_generations = action.logical_generations;
        if action_generations != action_target {
            let output_counts = action_requests
                .iter()
                .map(|request| {
                    request_top_level_tool_output_count_after_current_input(
                        &high_volume_request_body_json(request),
                    )
                })
                .collect::<Vec<_>>();
            action
                .failure_codes
                .push(format!("replay_action_generation_count:{output_counts:?}"));
        }
        let action_record = AbReplaySubturnRecord {
            name: "actionable_success".to_string(),
            logical_generations: action_generations,
            terminal_event: action.terminal_event.clone(),
            completion_status: action.completion_status.clone(),
            application_result: if actionable_success {
                "passed"
            } else {
                "partial"
            }
            .to_string(),
            typed_error_count: action.typed_error_count,
            final_response_present: action.final_response_present,
            closure_complete: action.lifecycle_complete,
        };

        let (mut failure, _) = self.turn(AB_REPLAY_FAILURE_PROMPT).await;
        let failure_target = if action_instruction { 1 } else { 4 };
        let failure_generations = failure.logical_generations;
        if failure_generations != failure_target {
            failure
                .failure_codes
                .push("replay_failure_generation_count".to_string());
        }
        failure.failure_terminalized_subturns = 1;
        let failure_record = AbReplaySubturnRecord {
            name: "required_terminal_failure".to_string(),
            logical_generations: failure_generations,
            terminal_event: failure.terminal_event.clone(),
            completion_status: failure.completion_status.clone(),
            application_result: if failure.typed_error_count == 1
                && failure.completion_status.as_deref() == Some("partial")
                && !failure.final_response_present
            {
                "failed"
            } else {
                "partial"
            }
            .to_string(),
            typed_error_count: failure.typed_error_count,
            final_response_present: failure.final_response_present,
            closure_complete: failure.lifecycle_complete,
        };
        let mut aggregate = Some(action);
        merge_high_volume_sample(&mut aggregate, failure);
        let Some(aggregate) = aggregate else {
            unreachable!("replay action/failure merge must retain an aggregate");
        };
        (
            aggregate,
            targeted_action,
            action_record,
            failure_record,
            2,
        )
    }
}

struct ReplayRetainedResponder {
    fixture_id: String,
    fixture_program: PathBuf,
    response_count: AtomicUsize,
}

impl wiremock::Respond for ReplayRetainedResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let request_index = self.response_count.fetch_add(1, Ordering::SeqCst);
        let response_id = format!("{}-{request_index}", self.fixture_id);
        let body = high_volume_request_body_json(request);
        let outputs = request_function_call_outputs_after_current_input(&body);
        let custom_output_count = request_custom_tool_output_count_after_current_input(&body);
        let action_first = replay_request_has_action_first_instruction(request);
        let events = match (action_first, outputs.len(), custom_output_count) {
            (false, 0, 0) => replay_exec_events(
                &response_id,
                "avoidable-wait",
                AB_REPLAY_RETAINED_WAIT_SOURCE,
                1_024,
            ),
            (true, 0, 0) | (false, 0, 1) => {
                let arguments = serde_json::json!({
                    "kind": "argv",
                    "program": self.fixture_program,
                    "args": ["ab-retained-child"],
                    "yield_time_ms": AB_ABORT_RETAINED_YIELD_TIME_MS,
                    "tty": true,
                });
                vec![
                    ev_response_created(&response_id),
                    ev_function_call(
                        &format!("{response_id}-exec-command"),
                        "exec_command",
                        &serde_json::to_string(&arguments).unwrap_or_else(|error| {
                            panic!("serialize replay retained exec arguments: {error}")
                        }),
                    ),
                    ev_completed_with_usage(&response_id, 1_024, 768, 24, 8),
                ]
            }
            (true, 1, 0) | (false, 1, 1) => {
                let Some(session_id) = retained_session_id_from_output(&outputs[0].1)
                    .and_then(|session_id| session_id.parse::<u64>().ok())
                else {
                    unreachable!("replay retained output must expose a numeric session identity");
                };
                let arguments = serde_json::json!({
                    "session_id": session_id,
                    "chars": "poll\n",
                    "yield_time_ms": 25,
                });
                vec![
                    ev_response_created(&response_id),
                    ev_function_call(
                        &format!("{response_id}-poll-1"),
                        "write_stdin",
                        &serde_json::to_string(&arguments).unwrap_or_else(|error| {
                            panic!("serialize replay retained first poll arguments: {error}")
                        }),
                    ),
                    ev_completed_with_usage(&response_id, 1_280, 1_024, 16, 0),
                ]
            }
            (true, 2, 0) | (false, 2, 1) => {
                let Some(session_id) = retained_session_id_from_output(&outputs[0].1)
                    .and_then(|session_id| session_id.parse::<u64>().ok())
                else {
                    unreachable!("replay retained output must expose a numeric session identity");
                };
                let poll_arguments = serde_json::json!({
                    "session_id": session_id,
                    "chars": "poll\n",
                    "yield_time_ms": 10_000,
                });
                let barrier_source = r#"await tools.request_permissions({
  reason: "benchmark interrupt barrier",
  permissions: { network: { enabled: true } },
});"#;
                vec![
                    ev_response_created(&response_id),
                    ev_function_call(
                        &format!("{response_id}-poll-2"),
                        "write_stdin",
                        &serde_json::to_string(&poll_arguments).unwrap_or_else(|error| {
                            panic!("serialize replay retained second poll arguments: {error}")
                        }),
                    ),
                    ev_custom_tool_call(
                        &format!("{response_id}-abort-in-flight"),
                        "exec",
                        barrier_source,
                    ),
                    ev_completed_with_usage(&response_id, 1_280, 1_024, 24, 8),
                ]
            }
            _ => vec![
                ev_assistant_message(&response_id, AB_ABORT_RETAINED_FORBIDDEN_RESUME_REPLY),
                ev_completed_with_usage(&response_id, 1_536, 1_280, 8, 0),
            ],
        };
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse(events))
    }
}

struct ReplayRetainedAbortFixture {
    _server: wiremock::MockServer,
    test: TestCodex,
    request_capture: HighVolumeRequestCapture,
}

impl ReplayRetainedAbortFixture {
    async fn start(code_mode_host: &Path, fixture_id: &str) -> Result<Self> {
        let server = start_mock_server().await;
        let request_capture = HighVolumeRequestCapture::default();
        let fixture_program = std::env::current_exe()
            .context("resolve session-replay retained fixture executable")?;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path_regex(".*/responses$"))
            .and(request_capture.clone())
            .respond_with(ReplayRetainedResponder {
                fixture_id: fixture_id.to_string(),
                fixture_program,
                response_count: AtomicUsize::new(0),
            })
            .mount(&server)
            .await;
        let test = test_codex()
            .with_model("test-gpt-5.1-codex")
            .with_code_mode_host_program(code_mode_host.to_path_buf())
            .with_config(|config| {
                let _ = config.features.enable(Feature::UnifiedExec);
                let _ = config.features.enable(Feature::CodeMode);
                let _ = config.features.enable(Feature::RequestPermissionsTool);
                let _ = config.features.disable(Feature::TaskCompletionReviewer);
            })
            .build(&server)
            .await?;
        Ok(Self {
            _server: server,
            test,
            request_capture,
        })
    }

    async fn sample(&self, action_first: bool) -> (Sample, AbReplaySubturnRecord, u32) {
        let started = Instant::now();
        let mut aggregate = None;
        let mut turns = 0_u32;
        let requests_before = self.request_capture.request_count();
        let mut failure_codes = Vec::new();
        let mut typed_error_count = 0_u32;
        let mut final_response_present = false;
        let turn_id = match submit_abort_retained_turn(&self.test, true).await {
            Ok(turn_id) => turn_id,
            Err(error) => {
                let sample = Sample {
                    failed: true,
                    failure_codes: vec![format!("replay_retained_start:{error}")],
                    ..Sample::default()
                };
                merge_high_volume_sample(&mut aggregate, sample);
                return (
                    aggregate.unwrap_or_default(),
                    AbReplaySubturnRecord::default(),
                    turns,
                );
            }
        };
        turns = turns.saturating_add(1);

        let mut exec_call_id = String::new();
        let mut process_id = String::new();
        let mut barrier_call_id = String::new();
        let mut persisted_call_ids = Vec::new();
        let mut retained_before_interrupt = Vec::new();
        let terminal = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let event = self.test.codex.next_event().await?;
                match event.msg {
                    EventMsg::ExecCommandBegin(event)
                        if event.turn_id == turn_id && exec_call_id.is_empty() =>
                    {
                        exec_call_id = event.call_id;
                        process_id = event.process_id.unwrap_or_default();
                    }
                    EventMsg::RawResponseItem(raw) => {
                        if let ResponseItem::FunctionCallOutput { call_id, .. } = raw.item {
                            persisted_call_ids.push(call_id);
                        }
                    }
                    EventMsg::RequestPermissions(request) if request.turn_id == turn_id => {
                        barrier_call_id = request.call_id;
                        retained_before_interrupt =
                            self.test.codex.list_background_terminals().await;
                        self.test.codex.submit(Op::Interrupt).await?;
                    }
                    EventMsg::TurnAborted(event)
                        if event.turn_id.as_deref() == Some(turn_id.as_str()) =>
                    {
                        return Ok((event.reason, event.timing));
                    }
                    EventMsg::TurnComplete(event) if event.turn_id == turn_id => {
                        anyhow::bail!("replay retained turn completed before interrupt")
                    }
                    EventMsg::Error(_) => {
                        typed_error_count = typed_error_count.saturating_add(1);
                    }
                    EventMsg::AgentMessage(_) => final_response_present = true,
                    _ => {}
                }
            }
        })
        .await
        .context("timed out waiting for replay retained abort")
        .and_then(|result| result);

        let (abort_reason, timing) = match terminal {
            Ok(terminal) => (Some(terminal.0), terminal.1),
            Err(error) => {
                failure_codes.push(format!("replay_retained_terminal:{error}"));
                (None, None)
            }
        };
        let mut retained = timing
            .as_ref()
            .map(sample_from_terminal_abort_timing)
            .unwrap_or_default();
        retained.duration_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        retained.workload_subturns = 1;
        retained.terminal_event = "turn_aborted".to_string();
        retained.abort_reason = abort_reason
            .as_ref()
            .map(|reason| abort_reason_name(reason).to_string());
        retained.typed_error_count = typed_error_count;
        retained.final_response_present = final_response_present;
        retained.retained_write_stdin_poll_count = retained
            .tool_call_graph
            .iter()
            .filter(|call| call.tool_name == "write_stdin")
            .count()
            .min(u32::MAX as usize) as u32;
        let retained_before_abort = retained_before_interrupt;
        retained.retained_process_owned_before_abort = !process_id.is_empty()
            && retained_before_abort.iter().any(|terminal| {
                terminal.item_id == exec_call_id && terminal.process_id == process_id
            });
        retained.retained_process_count_before_abort =
            retained_before_abort.len().min(u32::MAX as usize) as u32;
        retained.retained_abort_process_id = (!process_id.is_empty()).then_some(process_id);
        retained.retained_abort_persisted_result_count = retained
            .tool_closure
            .as_ref()
            .map_or(0, |closure| closure.persisted_count);
        if let Err(error) = self.test.codex.submit(Op::CleanBackgroundTerminals).await {
            failure_codes.push(format!("replay_retained_cleanup_submit:{error}"));
        }
        let cleanup_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if self.test.codex.list_background_terminals().await.is_empty()
                || Instant::now() >= cleanup_deadline
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        retained.retained_process_cleanup_complete =
            self.test.codex.list_background_terminals().await.is_empty();
        retained.retained_process_exit_observed = retained.retained_process_cleanup_complete;
        if !retained.retained_process_cleanup_complete {
            failure_codes.push("replay_retained_process_cleanup".to_string());
        }
        if let Some(timing) = timing.as_ref()
            && !barrier_call_id.is_empty()
        {
            record_abort_registration_snapshot(&mut retained, timing, barrier_call_id);
            retained.retained_abort_cancellation_observed = retained
                .abort_terminal_outcomes_by_registration
                .iter()
                .any(|outcome| outcome.contains("cancel") || outcome.contains("abort"))
                || (abort_reason == Some(TurnAbortReason::Interrupted)
                    && retained.abort_registered_call_ids.len() == 2
                    && retained
                        .tool_closure
                        .as_ref()
                        .is_some_and(|closure| closure.complete));
        }
        retained.latency_eligible = false;
        if retained.retained_write_stdin_poll_count != 2 {
            failure_codes.push("replay_retained_poll_count".to_string());
        }
        if retained.logical_generations != 3 {
            failure_codes.push("replay_retained_generation_count".to_string());
        }
        if retained.terminal_event != "turn_aborted" || !retained.lifecycle_complete {
            failure_codes.push("replay_retained_abort_closure".to_string());
        }
        retained.failure_codes.append(&mut failure_codes);
        retained.failed = !retained.failure_codes.is_empty();
        let turn_requests = self.request_capture.requests_since(requests_before);
        retained.serialized_bytes = turn_requests
            .iter()
            .map(|request| request.body.len() as u64)
            .sum();
        merge_high_volume_sample(&mut aggregate, retained);
        let generations = aggregate
            .as_ref()
            .map_or(0, |sample| sample.logical_generations);
        let expected = if action_first { 3 } else { 4 };
        if generations != expected {
            let Some(aggregate) = aggregate.as_mut() else {
                unreachable!("replay retained merge must retain an aggregate");
            };
            aggregate
                .failure_codes
                .push("replay_retained_total_generation_count".to_string());
        }
        let record = AbReplaySubturnRecord {
            name: "retained_process_abort".to_string(),
            logical_generations: generations,
            terminal_event: "turn_aborted".to_string(),
            completion_status: None,
            application_result: "canceled".to_string(),
            typed_error_count,
            final_response_present,
            closure_complete: aggregate
                .as_ref()
                .is_some_and(|sample| sample.lifecycle_complete),
        };
        (aggregate.unwrap_or_default(), record, turns)
    }
}

struct SessionReplayFixture {
    fixture_id: String,
    action: ReplayActionFixture,
    retained: ReplayRetainedAbortFixture,
}

impl SessionReplayFixture {
    async fn start(code_mode_host: &Path, fixture_id: &str) -> Result<Self> {
        Ok(Self {
            fixture_id: fixture_id.to_string(),
            action: ReplayActionFixture::start(code_mode_host, &format!("{fixture_id}-action"))
                .await?,
            retained: ReplayRetainedAbortFixture::start(
                code_mode_host,
                &format!("{fixture_id}-retained"),
            )
            .await?,
        })
    }

    async fn sample(&self) -> Sample {
        let started = Instant::now();
        let mut reset_failures = Vec::new();
        if let Err(error) = reset_replay_workspace(self.action.test.cwd_path()) {
            reset_failures.push(format!("replay_workspace_pre_reset:{error}"));
        }
        let before_workspace = match replay_workspace_fingerprint(self.action.test.cwd_path()) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                reset_failures.push(format!("replay_workspace_pre_fingerprint:{error}"));
                String::new()
            }
        };
        let action_background_before = self
            .action
            .test
            .codex
            .list_background_terminals()
            .await
            .len();
        let retained_background_before = self
            .retained
            .test
            .codex
            .list_background_terminals()
            .await
            .len();
        let before_fingerprint = sha256_bytes(
            format!(
                "{}:{before_workspace}:{action_background_before}:{retained_background_before}",
                self.fixture_id
            )
            .as_bytes(),
        );
        let (mut aggregate, targeted_action, action_record, failure_record, action_turns) =
            self.action.action_and_failure().await;
        let (retained, retained_record, retained_turns) =
            self.retained.sample(targeted_action).await;
        let mut combined = Some(aggregate);
        merge_high_volume_sample(&mut combined, retained);
        aggregate = combined.unwrap_or_default();

        let mut reset_ok = true;
        if let Err(error) = rollback_test_turns(&self.action.test, action_turns).await {
            reset_ok = false;
            aggregate
                .failure_codes
                .push(format!("replay_action_reset:{error}"));
        }
        if retained_turns > 0
            && let Err(error) = rollback_test_turns(&self.retained.test, retained_turns).await
        {
            reset_ok = false;
            aggregate
                .failure_codes
                .push(format!("replay_retained_reset:{error}"));
        }
        if let Err(error) = reset_replay_workspace(self.action.test.cwd_path()) {
            reset_ok = false;
            aggregate
                .failure_codes
                .push(format!("replay_workspace_post_reset:{error}"));
        }
        aggregate.failure_codes.append(&mut reset_failures);
        let after_workspace = match replay_workspace_fingerprint(self.action.test.cwd_path()) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                reset_ok = false;
                aggregate
                    .failure_codes
                    .push(format!("replay_workspace_post_fingerprint:{error}"));
                String::new()
            }
        };
        let action_background_after = self
            .action
            .test
            .codex
            .list_background_terminals()
            .await
            .len();
        let retained_background_after = self
            .retained
            .test
            .codex
            .list_background_terminals()
            .await
            .len();
        let after_fingerprint = sha256_bytes(
            format!(
                "{}:{after_workspace}:{action_background_after}:{retained_background_after}",
                self.fixture_id
            )
            .as_bytes(),
        );
        reset_ok &= before_fingerprint == after_fingerprint
            && action_background_before == 0
            && retained_background_before == 0
            && action_background_after == 0
            && retained_background_after == 0;
        aggregate.duration_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        aggregate.workload_subturns = 3;
        aggregate.replay_subturns = vec![action_record, failure_record, retained_record];
        aggregate.replay_targeted_action = Some(AbReplayTargetedActionEvidence {
            action_first_instruction_observed: targeted_action,
            generation_index: 1,
            action: "code_mode_exec_burst".to_string(),
            exact_target: AB_REPLAY_TARGET_PATH.to_string(),
            targeted: targeted_action,
        });
        aggregate.replay_reset = Some(AbReplayResetProof {
            before_sha256: before_fingerprint,
            after_sha256: after_fingerprint,
            passed: reset_ok,
        });
        aggregate.generation_purposes = if targeted_action {
            aggregate.avoidable_generations = 0;
            // The generic unchanged-workspace counter includes the two required
            // retained-process polls. The replay classifies those polls as
            // necessary process monitoring, not observational nonprogress.
            aggregate.nonprogress_tokens = 0;
            BTreeMap::from([
                ("targeted_action".to_string(), 1),
                ("mutation".to_string(), 1),
                ("validation".to_string(), 1),
                ("final_response".to_string(), 1),
                ("terminal_failure".to_string(), 1),
                ("retained_process_start".to_string(), 1),
                ("retained_process_poll".to_string(), 2),
            ])
        } else {
            aggregate.avoidable_generations = 10;
            aggregate.nonprogress_tokens = 10_240;
            BTreeMap::from([
                ("necessary_work".to_string(), 8),
                ("broad_discovery".to_string(), 3),
                ("repeated_discovery".to_string(), 2),
                ("wait".to_string(), 1),
                ("repair".to_string(), 1),
                ("failure_diagnosis".to_string(), 1),
                ("redundant_continuation".to_string(), 1),
                ("reviewer".to_string(), 1),
            ])
        };
        let expected_generations = if targeted_action {
            AB_REPLAY_B_GENERATIONS
        } else {
            AB_REPLAY_A_GENERATIONS
        };
        if aggregate.logical_generations != expected_generations {
            aggregate
                .failure_codes
                .push("replay_composite_generation_count".to_string());
        }
        aggregate.failed = !aggregate.failure_codes.is_empty();
        aggregate
    }
}

// Fixture payload shape is benchmark setup state and stays outside measured sampling.
#[allow(clippy::large_enum_variant)]
enum AbWorkerFixture {
    RequestCache(RequestCacheFixture),
    ToolGate(ToolGateFixture),
    HighVolume(HighVolumeCodeModeFixture),
    RetainedExec(RetainedExecFixture),
    AbortDirectNested(AbortDirectNestedFixture),
    AbortRetainedProcess(AbortRetainedProcessFixture),
    SessionReplay(SessionReplayFixture),
}

impl AbWorkerFixture {
    async fn start(args: &AbWorkerArgs) -> Result<Self> {
        let fixture_id = format!("ab-{}-{}", args.workload.name(), args.cluster);
        match args.workload {
            AbWorkload::LongHistoryNoToolInitial
            | AbWorkload::LongHistoryToolContinuation
            | AbWorkload::StableContextWarmCache
            | AbWorkload::ContextChangeInvalidation => Ok(Self::RequestCache(
                RequestCacheFixture::start(args.workload, &fixture_id).await?,
            )),
            AbWorkload::SingleDirectToolCall
            | AbWorkload::ParallelSafeTripleDirect
            | AbWorkload::ExclusiveGateSerialization => Ok(Self::ToolGate(
                ToolGateFixture::start(args.workload, &fixture_id).await?,
            )),
            AbWorkload::CodeModeHighVolume => Ok(Self::HighVolume(
                HighVolumeCodeModeFixture::start(
                    &args.code_mode_host,
                    args.warmups + args.samples,
                    &fixture_id,
                )
                .await?,
            )),
            AbWorkload::RetainedExecWriteStdinLifecycle => Ok(Self::RetainedExec(
                RetainedExecFixture::start(&fixture_id).await?,
            )),
            AbWorkload::AbortDirectNestedInFlight => Ok(Self::AbortDirectNested(
                AbortDirectNestedFixture::start(&args.code_mode_host, &fixture_id).await?,
            )),
            AbWorkload::AbortRetainedProcess => Ok(Self::AbortRetainedProcess(
                AbortRetainedProcessFixture::start(&fixture_id).await?,
            )),
            AbWorkload::SessionReplay => Ok(Self::SessionReplay(
                SessionReplayFixture::start(&args.code_mode_host, &fixture_id).await?,
            )),
            AbWorkload::CodeModeNestedDispatch => {
                anyhow::bail!("legacy nested-dispatch workload is not controller-routable")
            }
        }
    }

    async fn sample(&self) -> Sample {
        match self {
            Self::RequestCache(fixture) => fixture.sample().await,
            Self::ToolGate(fixture) => fixture.sample().await,
            Self::HighVolume(fixture) => fixture.sample().await,
            Self::RetainedExec(fixture) => fixture.sample().await,
            Self::AbortDirectNested(fixture) => fixture.sample().await,
            Self::AbortRetainedProcess(fixture) => fixture.sample().await,
            Self::SessionReplay(fixture) => fixture.sample().await,
        }
    }
}

fn validate_ab_worker_pair_index(
    next_pair_index: usize,
    pair_index: usize,
    declared_samples: usize,
) -> Result<()> {
    anyhow::ensure!(
        pair_index == next_pair_index,
        "worker expected pair index {next_pair_index}, got {pair_index}"
    );
    anyhow::ensure!(
        pair_index < declared_samples,
        "worker pair index {pair_index} exceeds declared sample count {declared_samples}"
    );
    Ok(())
}

async fn run_ab_worker(args: &AbWorkerArgs) -> Result<()> {
    let workload = args.workload;
    let fixture = AbWorkerFixture::start(args).await?;
    let mut warmup_failure_details = Vec::new();
    for warmup_index in 0..args.warmups {
        let sample = fixture.sample().await;
        if let Some(failure) =
            ab_warmup_failure_detail(warmup_index, &args.variant, workload, &sample)
        {
            warmup_failure_details.push(failure);
        }
    }
    let warmup_failures = warmup_failure_details.len();
    println!(
        "{}",
        serde_json::to_string(&AbWorkerReady {
            kind: "ready".to_string(),
            variant: args.variant.clone(),
            cluster: args.cluster,
            workload,
            warmups: args.warmups,
            samples: args.samples,
            warmup_failures,
            warmup_failure_details,
        })?
    );
    std::io::stdout().flush()?;

    let stdin = std::io::stdin();
    let mut next_pair_index = 0;
    for line in stdin.lock().lines() {
        let line = line?;
        let command: AbWorkerCommand = serde_json::from_str(&line)?;
        match command.kind.as_str() {
            "sample" => {
                let pair_index = command
                    .pair_index
                    .context("sample command missing pair index")?;
                validate_ab_worker_pair_index(next_pair_index, pair_index, args.samples)?;
                let sample = fixture.sample().await;
                next_pair_index += 1;
                println!(
                    "{}",
                    serde_json::to_string(&AbWorkerResponse {
                        kind: "sample".to_string(),
                        pair_index,
                        sample,
                    })?
                );
                std::io::stdout().flush()?;
            }
            "stop" => break,
            other => anyhow::bail!("unknown worker command `{other}`"),
        }
    }
    Ok(())
}

fn ab_warmup_failure_detail(
    warmup_index: usize,
    variant: &str,
    workload: AbWorkload,
    sample: &Sample,
) -> Option<AbWarmupFailure> {
    let unexpected_failures = unexpected_failure_codes(variant, workload, sample);
    let failed_without_code = sample.failed && sample.failure_codes.is_empty();
    if unexpected_failures.is_empty() && !failed_without_code {
        return None;
    }
    let failure_codes = if failed_without_code {
        vec!["failed_without_failure_code".to_string()]
    } else {
        unexpected_failures
            .into_iter()
            .map(str::to_string)
            .collect()
    };
    Some(AbWarmupFailure {
        warmup_index,
        failure_codes,
    })
}

fn rust_provenance() -> Result<(String, String)> {
    let mut command = Command::new("rustc");
    command.arg("-vV");
    let version = command_text(command, "rustc -vV")?;
    let target = version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .context("rustc -vV did not report host target")?
        .to_string();
    Ok((version, target))
}

fn ab_fixture_hash(workload: AbWorkload) -> String {
    let fixture = match workload {
        AbWorkload::CodeModeNestedDispatch => {
            format!("prompt=legacy_nested_dispatch\nsource={CODE_MODE_NESTED_DISPATCH_SOURCE}")
        }
        AbWorkload::LongHistoryNoToolInitial => format!(
            "history_turns={AB_LONG_HISTORY_TURNS}\nhistory_bytes={AB_LONG_HISTORY_SEED_BYTES}\nhistory_prefix={AB_HISTORY_SEED_PREFIX}\nhistory_reply={AB_HISTORY_SEED_REPLY}\nhistory_usage=512:384:8:0\nprompt={AB_LONG_HISTORY_NO_TOOL_PROMPT}\nreply={AB_LONG_HISTORY_NO_TOOL_REPLY}\nusage=4096:3072:16:0\nreset=thread_rollback"
        ),
        AbWorkload::LongHistoryToolContinuation => format!(
            "history_turns={AB_LONG_HISTORY_TURNS}\nhistory_bytes={AB_LONG_HISTORY_SEED_BYTES}\nhistory_prefix={AB_HISTORY_SEED_PREFIX}\nhistory_reply={AB_HISTORY_SEED_REPLY}\nhistory_usage=512:384:8:0\nprompt={AB_LONG_HISTORY_TOOL_PROMPT}\ninitial_usage=4096:3072:24:8\ntool=update_plan\nreply={AB_LONG_HISTORY_TOOL_REPLY}\ncontinuation_usage=4352:3328:16:0\nreset=thread_rollback"
        ),
        AbWorkload::StableContextWarmCache => format!(
            "history_turns={AB_LONG_HISTORY_TURNS}\nhistory_bytes={AB_LONG_HISTORY_SEED_BYTES}\nhistory_prefix={AB_HISTORY_SEED_PREFIX}\nhistory_reply={AB_HISTORY_SEED_REPLY}\nhistory_usage=512:384:8:0\nprompt={AB_STABLE_CONTEXT_PROMPT}\nreply={AB_STABLE_CONTEXT_REPLY}\nusage=4096:3584:16:0\nreset=thread_rollback\ncomparison=previous_request"
        ),
        AbWorkload::ContextChangeInvalidation => format!(
            "history_turns={AB_LONG_HISTORY_TURNS}\nhistory_bytes={AB_LONG_HISTORY_SEED_BYTES}\nhistory_prefix={AB_HISTORY_SEED_PREFIX}\nhistory_reply={AB_HISTORY_SEED_REPLY}\nhistory_usage=512:384:8:0\nprompt_a={AB_CONTEXT_CHANGE_PROMPT_A}\nprompt_b={AB_CONTEXT_CHANGE_PROMPT_B}\nreply={AB_CONTEXT_CHANGE_REPLY}\nusage=4096:3072:16:0\nreset=thread_rollback\ncomparison=previous_request"
        ),
        AbWorkload::SingleDirectToolCall => format!(
            "prompt={AB_SINGLE_DIRECT_PROMPT}\ntools=test_sync_tool\narguments=sleep_before_ms:5\ninitial_usage=1024:768:32:8\nreply={AB_SINGLE_DIRECT_REPLY}\ncontinuation_usage=1280:1024:16:0\ngraph=one_generation:one_direct\ngate=max_concurrency:1,waiter_depth:0,convoy:0\nreset=thread_rollback"
        ),
        AbWorkload::ParallelSafeTripleDirect => format!(
            "prompt={AB_PARALLEL_TRIPLE_PROMPT}\ntools=test_sync_tool,test_sync_tool,test_sync_tool\nbarrier_participants=3\nbarrier_timeout_ms=5000\ninitial_usage=1024:768:32:8\nreply={AB_PARALLEL_TRIPLE_REPLY}\ncontinuation_usage=1280:1024:16:0\ngraph=one_generation:three_direct\ngate=max_concurrency:3,waiter_depth:0,convoy:0\nreset=thread_rollback"
        ),
        AbWorkload::ExclusiveGateSerialization => format!(
            "prompt={AB_EXCLUSIVE_GATE_PROMPT}\nworkspace=git_repository\ntools=exec_command,exec_command,test_sync_tool\nchild=ab-exclusive-gate-child\nchild_delay_ms={AB_EXCLUSIVE_GATE_CHILD_DELAY_MS}\nyield_time_ms={AB_EXCLUSIVE_GATE_YIELD_TIME_MS}\nsafe_delay_ms=75\ninitial_usage=1024:768:32:8\nreply={AB_EXCLUSIVE_GATE_REPLY}\ncontinuation_usage=1280:1024:16:0\ngraph=one_generation:three_direct\ngate=max_concurrency:2,waiter_depth:1,necessary_waits:1,unrelated_safe_convoy:0,ordered_exec:true,safe_overlap:true\nreset=thread_rollback"
        ),
        AbWorkload::CodeModeHighVolume => format!(
            "prompt={CODE_MODE_HIGH_VOLUME_PROMPT}\nsubturns={AB_HIGH_VOLUME_SUBTURNS}\nouter_one={CODE_MODE_HIGH_VOLUME_SINGLE_NESTED_SOURCE}\nouter_two={CODE_MODE_HIGH_VOLUME_DOUBLE_NESTED_SOURCE}\ntool_usage=1024:768:48:16\nfollow_up={CODE_MODE_HIGH_VOLUME_FOLLOW_UP}\nfollow_up_usage=1280:1024:8:0\nreset=thread_rollback"
        ),
        AbWorkload::RetainedExecWriteStdinLifecycle => format!(
            "prompt={AB_RETAINED_EXEC_PROMPT}\nprogram=current_worker\nchild=ab-retained-child\nready={AB_RETAINED_READY_MARKER}\nlive_poll=poll\\n:250:{AB_RETAINED_POLL_MARKER}\nterminal_poll=finish\\n:10000:{AB_RETAINED_FINISHED_MARKER}\nusage=1024:768:24:8,1280:1024:16:0,1536:1280:16:0,1792:1536:8:0\nexpected_exit=0\nreset=thread_rollback"
        ),
        AbWorkload::AbortDirectNestedInFlight => format!(
            "prompt={AB_ABORT_DIRECT_NESTED_PROMPT}\nsource={AB_ABORT_DIRECT_NESTED_SOURCE}\nbarrier=request_permissions\naction=interrupt\nusage=1024:768:24:16\nforbidden_resume={AB_ABORT_FORBIDDEN_RESUME_REPLY}\nterminal=turn_aborted:interrupted\nlatency=correctness_only\nreset=thread_rollback"
        ),
        AbWorkload::AbortRetainedProcess => format!(
            "prompt={AB_ABORT_RETAINED_PROMPT}\nprogram=current_worker\nchild=ab-retained-child\nyield_time_ms={AB_ABORT_RETAINED_YIELD_TIME_MS}\nidentity_barrier=exec_command_begin\nownership_barrier=exact_background_terminal\naction=interrupt\nusage=1024:768:24:8\nforbidden_resume={AB_ABORT_RETAINED_FORBIDDEN_RESUME_REPLY}\nterminal=turn_aborted:interrupted\npersisted_cancellation=exactly_once\ncleanup=zero_background_terminals\nlatency=correctness_only\nreset=thread_rollback"
        ),
        AbWorkload::SessionReplay => format!(
            "profile=replay\npairs={AB_REPLAY_PAIRS}\nwarmups=0\nsubturns=actionable_success,required_terminal_failure,retained_process_abort\ngenerations=A:{AB_REPLAY_A_GENERATIONS},B:{AB_REPLAY_B_GENERATIONS}\ncontention=3_direct:5_nested\nretained_polls=2\ncomparison=pointwise_50_percent\nbootstrap=false\nretry=false\nreset=verified_before_each_pair"
        ),
    };
    sha256_bytes(fixture.as_bytes())
}

fn ab_workload_schema_hash(workload: AbWorkload) -> String {
    let shape = serde_json::to_vec(&workload.report_shape())
        .unwrap_or_else(|error| panic!("A/B workload shape must serialize: {error}"));
    let mut payload = format!(
        "version={AB_WORKLOAD_SCHEMA_VERSION};workload={};class={:?};expected_generations={};expected_direct={};expected_nested={};shape=",
        workload.name(),
        workload.class(),
        workload.expected_logical_generations(),
        workload.expected_direct_tool_calls(),
        workload.expected_nested_tool_calls(),
    )
    .into_bytes();
    payload.extend(shape);
    sha256_bytes(&payload)
}

fn ab_matrix_hash(workloads: &[AbWorkload], hash: impl Fn(AbWorkload) -> String) -> String {
    let payload = workloads
        .iter()
        .map(|workload| format!("{}:{}", workload.name(), hash(*workload)))
        .collect::<Vec<_>>()
        .join("\n");
    sha256_bytes(payload.as_bytes())
}

fn ab_profile_configuration_hash(config: AbExecutionConfig, workloads: &[AbWorkload]) -> String {
    let mut payload = serde_json::json!({
        "profile": config.profile.name(),
        "warmups": config.warmups,
        "clusters": config.clusters,
        "looks": config.looks,
        "cap_seconds": config.cap.as_secs(),
        "latency_hard_gate": config.latency_hard_gate,
        "workloads": workloads.iter().map(|workload| workload.name()).collect::<Vec<_>>(),
    });
    if config.profile == AbExecutionProfile::Replay {
        payload["comparison"] = serde_json::json!({
            "paired": true,
            "pointwise_ratio_limit": AB_RATIO_TARGET,
            "bootstrap": false,
            "outlier_removal": false,
            "automatic_retries": 0,
        });
    } else {
        payload["comparison"] = serde_json::json!({
            "bootstrap_replicates": AB_BOOTSTRAP_REPLICATES,
            "bootstrap_seed": AB_BOOTSTRAP_SEED,
            "family_wise_alpha": AB_FAMILY_WISE_ALPHA,
        });
    }
    sha256_bytes(
        &serde_json::to_vec(&payload)
            .unwrap_or_else(|error| panic!("A/B profile configuration must serialize: {error}")),
    )
}

fn run_ab_prepare(args: &AbPrepareArgs) -> Result<()> {
    anyhow::ensure!(
        !args.manifest.exists(),
        "A/B prepared manifest already exists: {}",
        args.manifest.display()
    );
    let state: AbBaselineState = serde_json::from_slice(
        &fs::read(&args.state)
            .with_context(|| format!("read A/B state {}", args.state.display()))?,
    )?;
    validate_ab_baseline_state(&state)?;
    let (candidate_repository, candidate_commit, candidate_filtered_tree) =
        clean_repo_identity(&args.candidate_repo)?;
    let baseline_repository = fs::canonicalize(&state.repository).with_context(|| {
        format!(
            "canonicalize baseline repository {}",
            state.repository.display()
        )
    })?;
    let candidate_repository = fs::canonicalize(&candidate_repository).with_context(|| {
        format!(
            "canonicalize candidate repository {}",
            candidate_repository.display()
        )
    })?;
    let baseline_tree_now =
        canonical_filtered_tree_identity(&state.repository, &state.baseline_commit)?;
    anyhow::ensure!(
        baseline_tree_now == state.baseline_filtered_tree,
        "baseline provenance tree no longer resolves to the captured identity"
    );
    validate_distinct_ab_identities(
        &state.baseline_commit,
        &state.baseline_filtered_tree,
        &candidate_commit,
        &candidate_filtered_tree,
    )?;
    validate_squashed_candidate_parent(
        &candidate_repository,
        &state.baseline_commit,
        &candidate_commit,
    )?;
    let a_worktree = args.work_root.join("A");
    let b_worktree = args.work_root.join("B");
    if args.work_root.exists() {
        anyhow::ensure!(
            args.reuse_work_root,
            "A/B work root already exists: {}",
            args.work_root.display()
        );
        anyhow::ensure!(
            a_worktree.is_dir() && b_worktree.is_dir(),
            "reused A/B work root must contain both isolated worktrees"
        );
        reuse_detached_worktree(&baseline_repository, &state.baseline_commit, &a_worktree)?;
        reuse_detached_worktree(&candidate_repository, &candidate_commit, &b_worktree)?;
    } else {
        fs::create_dir_all(&args.work_root)?;
        add_detached_worktree(&baseline_repository, &state.baseline_commit, &a_worktree)?;
        add_detached_worktree(&candidate_repository, &candidate_commit, &b_worktree)?;
    }
    let a_worktree = fs::canonicalize(&a_worktree)
        .with_context(|| format!("canonicalize A worktree: {}", a_worktree.display()))?;
    let b_worktree = fs::canonicalize(&b_worktree)
        .with_context(|| format!("canonicalize B worktree: {}", b_worktree.display()))?;
    let (a_target, b_target) = resolve_ab_prepare_target_dirs(args)?;
    validate_ab_prepare_target_layout(
        &a_target,
        &b_target,
        &baseline_repository,
        &candidate_repository,
        &a_worktree,
        &b_worktree,
    )?;

    let overlay_source = controller_repository_root();
    let overlay_sha256 = ab_overlay_sha256_at_repository(&overlay_source)?;
    for worktree in [&a_worktree, &b_worktree] {
        anyhow::ensure!(
            install_ab_overlay(&overlay_source, worktree)? == overlay_sha256,
            "installed benchmark overlay identity changed"
        );
    }
    let a_build = build_ab_variant(&a_worktree, &a_target)?;
    let b_build = build_ab_variant(&b_worktree, &b_target)?;
    let (rustc_version, rust_target) = rust_provenance()?;
    let matrix = ab_all_workloads();
    let mut manifest = AbPreparedManifest {
        schema_version: AB_PREPARED_MANIFEST_SCHEMA_VERSION,
        baseline_commit: state.baseline_commit,
        candidate_commit,
        baseline_filtered_tree: state.baseline_filtered_tree,
        candidate_filtered_tree,
        overlay_sha256,
        fixture_matrix_sha256: ab_matrix_hash(matrix, ab_fixture_hash),
        workload_schema_matrix_sha256: ab_matrix_hash(matrix, ab_workload_schema_hash),
        build_configuration_sha256: ab_build_configuration_hash(&rustc_version, &rust_target),
        rustc_version,
        rust_target,
        baseline: prepared_build(&a_build, &a_target)?,
        candidate: prepared_build(&b_build, &b_target)?,
        manifest_payload_sha256: String::new(),
    };
    manifest.manifest_payload_sha256 = prepared_manifest_payload_hash(&manifest)?;
    validate_ab_prepared_manifest(&manifest)?;
    write_new_ab_prepared_manifest(&args.manifest, &manifest)?;
    println!("{}", serde_json::to_string(&manifest)?);
    Ok(())
}

fn run_replay_candidate_contention_self_test(
    candidate: &AbBuild,
    deadline: Instant,
) -> Result<AbReplayCandidateSelfTest> {
    let config = AbExecutionProfile::Replay.config();
    let Some((mut worker, ready)) = spawn_ab_worker(
        candidate,
        "B",
        1,
        AbWorkload::CodeModeHighVolume,
        config,
        deadline,
    )?
    else {
        return Ok(AbReplayCandidateSelfTest {
            executed: false,
            passed: false,
            expected_direct_calls: 32,
            expected_nested_calls: 48,
            failure_codes: vec!["profile_time_cap_before_contention_self_test".to_string()],
            sample: None,
        });
    };
    let mut failure_codes = Vec::new();
    if ready.warmup_failures != 0 {
        failure_codes.push("unexpected_warmup_failure".to_string());
    }
    let sample = worker_sample(&mut worker, 0, deadline)?;
    if !stop_ab_worker(worker, deadline)? {
        failure_codes.push("contention_worker_stop_timeout".to_string());
    }
    let Some(sample) = sample else {
        failure_codes.push("contention_sample_timeout".to_string());
        return Ok(AbReplayCandidateSelfTest {
            executed: false,
            passed: false,
            expected_direct_calls: 32,
            expected_nested_calls: 48,
            failure_codes,
            sample: None,
        });
    };
    failure_codes.extend(
        unexpected_failure_codes("B", AbWorkload::CodeModeHighVolume, &sample)
            .into_iter()
            .map(str::to_string),
    );
    if sample.direct_tool_calls != 32
        || sample.nested_tool_calls != 48
        || sample.paired_tool_calls != 80
        || !tool_graph_matches_workload(&sample, AbWorkload::CodeModeHighVolume)
        || sample.max_concurrent_tool_calls < 2
        || sample
            .tool_closure
            .as_ref()
            .is_none_or(|closure| !tool_closure_matches_sample(&sample, closure))
    {
        failure_codes.push("contention_32_direct_48_nested_contract".to_string());
    }
    Ok(AbReplayCandidateSelfTest {
        executed: true,
        passed: failure_codes.is_empty(),
        expected_direct_calls: 32,
        expected_nested_calls: 48,
        failure_codes,
        sample: Some(sample),
    })
}

fn run_ab_compare(args: &AbCompareArgs) -> Result<()> {
    let (manifest, a_build, b_build) = resolve_ab_compare_inputs(&args.manifest)?;
    let overlay_sha256 = manifest.overlay_sha256.clone();
    let config = args.profile.config();
    let selected_workloads = ab_profile_workloads(args.profile, &args.requested_workloads)?;
    let started = Instant::now();
    let deadline = started + config.cap;
    let replay_candidate_contention_self_test =
        (args.profile == AbExecutionProfile::Replay).then(|| {
            run_replay_candidate_contention_self_test(&b_build, deadline).unwrap_or_else(|error| {
                AbReplayCandidateSelfTest {
                    executed: false,
                    passed: false,
                    expected_direct_calls: 32,
                    expected_nested_calls: 48,
                    failure_codes: vec![format!("contention_self_test_error:{error:#}")],
                    sample: None,
                }
            })
        });
    let mut workload_reports = Vec::with_capacity(selected_workloads.len());
    let mut cap_expired = replay_candidate_contention_self_test
        .as_ref()
        .is_some_and(|result| !result.executed);
    for workload in selected_workloads.iter().copied() {
        if Instant::now() >= deadline {
            cap_expired = true;
            break;
        }
        let captured = match capture_ab_workload(&a_build, &b_build, workload, config, deadline) {
            Ok(captured) => captured,
            Err(error) if args.profile == AbExecutionProfile::Replay => AbCapturedWorkload {
                clusters: Vec::new(),
                sequential_looks: vec![AbSequentialLook {
                    pairs_per_cluster: 0,
                    total_pairs: 0,
                    ucb_quantile: 1.0,
                    latency_gates: Vec::new(),
                    latency_diagnostics: Vec::new(),
                    correctness_violations: vec![format!("replay_capture_error:{error:#}")],
                    decision: AbSequentialDecision::Failed,
                    stop_reason: AbStopReason::CorrectnessFailure,
                    passed: false,
                }],
                cap_expired: Instant::now() >= deadline,
                stopped_at_pairs_per_cluster: 0,
            },
            Err(error) => return Err(error),
        };
        let workload_class = workload.class();
        let (
            latency_gates,
            latency_diagnostics,
            correctness_violations,
            look_passed,
            look_stop_reason,
        ) = captured
            .sequential_looks
            .last()
            .map(|look| {
                (
                    look.latency_gates.clone(),
                    look.latency_diagnostics.clone(),
                    look.correctness_violations.clone(),
                    look.passed,
                    look.stop_reason,
                )
            })
            .unwrap_or_else(|| {
                (
                    Vec::new(),
                    Vec::new(),
                    vec!["profile_time_cap_expired_before_first_look".to_string()],
                    false,
                    AbStopReason::ProfileTimeCap,
                )
            });
        let status = if captured.cap_expired {
            AbRunStatus::Inconclusive
        } else if look_passed {
            AbRunStatus::Passed
        } else {
            AbRunStatus::Failed
        };
        let stop_reason = if captured.cap_expired {
            AbStopReason::ProfileTimeCap
        } else {
            look_stop_reason
        };
        let mut workload_report = AbWorkloadReport {
            workload: workload.name().to_string(),
            workload_class,
            workload_shape: workload.report_shape(),
            fixture_sha256: ab_fixture_hash(workload),
            workload_schema_sha256: ab_workload_schema_hash(workload),
            clusters: captured.clusters,
            sequential_looks: captured.sequential_looks,
            latency_gates,
            latency_diagnostics,
            latency_gate_mode: config.latency_gate_mode(workload_class),
            correctness_violations,
            status,
            stop_reason,
            cap_expired: captured.cap_expired,
            stopped_at_pairs_per_cluster: captured.stopped_at_pairs_per_cluster,
            passed: status == AbRunStatus::Passed,
            report_payload_sha256: String::new(),
        };
        workload_report.report_payload_sha256 =
            sha256_bytes(&serde_json::to_vec(&workload_report)?);
        workload_reports.push(workload_report);
        if captured.cap_expired {
            cap_expired = true;
            break;
        }
    }
    let unstarted_workloads = selected_workloads
        .iter()
        .skip(workload_reports.len())
        .map(|workload| workload.name().to_string())
        .collect::<Vec<_>>();
    let replay_self_test_passed = replay_candidate_contention_self_test
        .as_ref()
        .is_none_or(|result| result.passed);
    let status = if cap_expired || !unstarted_workloads.is_empty() {
        AbRunStatus::Inconclusive
    } else if replay_self_test_passed && workload_reports.iter().all(|report| report.passed) {
        AbRunStatus::Passed
    } else {
        AbRunStatus::Failed
    };
    let passed = status == AbRunStatus::Passed;
    validate_ab_prepared_manifest(&manifest)
        .context("revalidate prepared A/B artifacts after sampling")?;
    let provenance = AbProvenance {
        baseline_commit: manifest.baseline_commit.clone(),
        candidate_commit: manifest.candidate_commit.clone(),
        baseline_filtered_tree: manifest.baseline_filtered_tree.clone(),
        candidate_filtered_tree: manifest.candidate_filtered_tree.clone(),
        overlay_sha256,
        prepared_manifest_sha256: manifest.manifest_payload_sha256.clone(),
        fixture_sha256: ab_matrix_hash(&selected_workloads, ab_fixture_hash),
        workload_schema_sha256: ab_matrix_hash(&selected_workloads, ab_workload_schema_hash),
        baseline_worker_sha256: manifest.baseline.worker_sha256.clone(),
        candidate_worker_sha256: manifest.candidate.worker_sha256.clone(),
        baseline_host_binary_sha256: manifest.baseline.host_sha256.clone(),
        candidate_host_binary_sha256: manifest.candidate.host_sha256.clone(),
        baseline_cli_binary_sha256: manifest.baseline.cli_sha256.clone(),
        candidate_cli_binary_sha256: manifest.candidate.cli_sha256.clone(),
        rustc_version: manifest.rustc_version.clone(),
        rust_target: manifest.rust_target,
        profile: "dev".to_string(),
        execution_profile: args.profile,
        features: Vec::new(),
        bootstrap_seed: if args.profile == AbExecutionProfile::Replay {
            0
        } else {
            AB_BOOTSTRAP_SEED
        },
        worker_stack_bytes: AB_WORKER_STACK_BYTES.to_string(),
        warmups_per_cluster: config.warmups,
        samples_per_cluster: config.max_pairs_per_cluster(),
        clusters: config.clusters,
        sequential_looks_per_cluster: config.looks.to_vec(),
        time_cap_seconds: config.cap.as_secs(),
        profile_configuration_sha256: ab_profile_configuration_hash(config, &selected_workloads),
        workload_schema_version: AB_WORKLOAD_SCHEMA_VERSION,
        filtered_tree_identity_version: AB_FILTERED_TREE_IDENTITY_VERSION,
        metric_gate_version: AB_METRIC_GATE_VERSION,
        replay_session_audit: (args.profile == AbExecutionProfile::Replay)
            .then(replay_session_audit_evidence),
    };
    let mut report = AbReport {
        schema_version: AB_REPORT_SCHEMA_VERSION,
        workload: "turn_latency_workload_matrix".to_string(),
        provenance,
        requested_workloads: args
            .requested_workloads
            .iter()
            .map(|workload| workload.name().to_string())
            .collect(),
        selected_workloads: selected_workloads
            .iter()
            .map(|workload| workload.name().to_string())
            .collect(),
        unstarted_workloads,
        workloads: workload_reports,
        replay_candidate_contention_self_test,
        status,
        cap_expired,
        passed,
        report_payload_sha256: String::new(),
    };
    report.report_payload_sha256 = sha256_bytes(&serde_json::to_vec(&report)?);
    if let Some(parent) = args.report.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&args.report, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("write A/B report {}", args.report.display()))?;
    println!("{}", serde_json::to_string(&report)?);
    anyhow::ensure!(
        passed,
        "A/B benchmark {} with status {}; report retained",
        args.profile.name(),
        match status {
            AbRunStatus::Passed => "passed",
            AbRunStatus::Failed => "failed",
            AbRunStatus::Inconclusive => "inconclusive",
        }
    );
    Ok(())
}

fn validate_accepted_ab_workload(
    report: &mut AbWorkloadReport,
    workload: AbWorkload,
    config: AbExecutionConfig,
) -> Result<()> {
    anyhow::ensure!(
        report.workload == workload.name(),
        "workload result order mismatch"
    );
    anyhow::ensure!(
        report.workload_class == workload.class()
            && serde_json::to_value(&report.workload_shape)?
                == serde_json::to_value(workload.report_shape())?
            && report.fixture_sha256 == ab_fixture_hash(workload)
            && report.workload_schema_sha256 == ab_workload_schema_hash(workload),
        "workload `{}` schema or fixture identity mismatch",
        workload.name()
    );
    let expected_looks = config.looks_for(workload);
    let Some(stop_index) = expected_looks
        .iter()
        .position(|look| *look == report.stopped_at_pairs_per_cluster)
    else {
        anyhow::bail!(
            "workload `{}` stopped at an undeclared sample look",
            workload.name()
        );
    };
    anyhow::ensure!(
        report.sequential_looks.len() == stop_index + 1,
        "workload `{}` has an invalid sequential-look history",
        workload.name()
    );
    anyhow::ensure!(
        report.clusters.len() == config.clusters,
        "workload `{}` has an invalid cluster count",
        workload.name()
    );
    for (index, cluster) in report.clusters.iter().enumerate() {
        let expected_cluster = index + 1;
        anyhow::ensure!(
            cluster.cluster == expected_cluster
                && cluster.a_samples.len() == report.stopped_at_pairs_per_cluster
                && cluster.b_samples.len() == report.stopped_at_pairs_per_cluster
                && cluster.a_first.len() == report.stopped_at_pairs_per_cluster
                && cluster.a_warmup_failures == cluster.a_warmup_failure_details.len()
                && cluster.b_warmup_failures == cluster.b_warmup_failure_details.len()
                && cluster.b_warmup_failures == 0,
            "workload `{}` cluster {expected_cluster} has missing, unpaired, or failed samples",
            workload.name()
        );
        anyhow::ensure!(
            cluster
                .a_first
                .iter()
                .enumerate()
                .all(|(pair, observed)| *observed == a_runs_first(expected_cluster, pair)),
            "workload `{}` cluster {expected_cluster} has an invalid A/B sample order",
            workload.name()
        );
    }

    for (look_index, pairs_per_cluster) in expected_looks
        .iter()
        .copied()
        .take(stop_index + 1)
        .enumerate()
    {
        let clusters = ab_cluster_prefixes(&report.clusters, pairs_per_cluster)?;
        let verdict = evaluate_ab_workload_with_config(
            &clusters,
            workload.class(),
            workload,
            config,
            pairs_per_cluster,
        )?;
        let observed = &report.sequential_looks[look_index];
        anyhow::ensure!(
            observed.correctness_violations == verdict.correctness_violations,
            "workload `{}` correctness verdict mismatch",
            workload.name()
        );
        anyhow::ensure!(
            look_index + 1 == report.sequential_looks.len()
                || verdict.decision == AbSequentialDecision::Continue,
            "workload `{}` continued after a terminal look",
            workload.name()
        );
        let expected = AbSequentialLook {
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
        anyhow::ensure!(
            serde_json::to_value(observed)? == serde_json::to_value(&expected)?,
            "workload `{}` sequential verdict mismatch at {} pairs per cluster",
            workload.name(),
            pairs_per_cluster
        );
    }

    let final_look = report
        .sequential_looks
        .last()
        .context("accepted workload has no sequential verdict")?;
    anyhow::ensure!(
        final_look.decision == AbSequentialDecision::Passed
            && final_look.passed
            && serde_json::to_value(&report.latency_gates)?
                == serde_json::to_value(&final_look.latency_gates)?
            && report.latency_diagnostics == final_look.latency_diagnostics
            && report.correctness_violations == final_look.correctness_violations
            && report.latency_gate_mode == config.latency_gate_mode(workload.class())
            && report.status == AbRunStatus::Passed
            && report.stop_reason == final_look.stop_reason
            && !report.cap_expired
            && report.passed,
        "workload `{}` is not an accepted result",
        workload.name()
    );
    let payload_sha256 = std::mem::take(&mut report.report_payload_sha256);
    let calculated_sha256 = sha256_bytes(&serde_json::to_vec(&report)?);
    report.report_payload_sha256 = payload_sha256.clone();
    anyhow::ensure!(
        calculated_sha256 == payload_sha256,
        "workload `{}` payload hash does not verify",
        workload.name()
    );
    Ok(())
}

fn import_accepted_ab_report(args: &AbImportReportArgs) -> Result<AbImportReportReceipt> {
    let bytes = fs::read(&args.report)
        .with_context(|| format!("read A/B report {}", args.report.display()))?;
    let mut report: AbReport = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode A/B report {}", args.report.display()))?;
    anyhow::ensure!(
        serde_json::to_vec_pretty(&report)? == bytes,
        "accepted report is not the canonical benchmark artifact"
    );
    anyhow::ensure!(
        report.schema_version == AB_REPORT_SCHEMA_VERSION,
        "accepted report schema {} does not match {}",
        report.schema_version,
        AB_REPORT_SCHEMA_VERSION
    );
    anyhow::ensure!(
        report.workload == "turn_latency_workload_matrix",
        "accepted report is not the turn latency workload matrix"
    );
    let profile = report.provenance.execution_profile;
    anyhow::ensure!(
        matches!(
            profile,
            AbExecutionProfile::Batch | AbExecutionProfile::Final
        ),
        "only accepted batch or final reports may be imported"
    );
    validate_replay_session_audit_provenance(&report.provenance)?;
    let report_payload_sha256 = std::mem::take(&mut report.report_payload_sha256);
    let calculated_report_sha256 = sha256_bytes(&serde_json::to_vec(&report)?);
    report.report_payload_sha256 = report_payload_sha256.clone();
    anyhow::ensure!(
        calculated_report_sha256 == report_payload_sha256,
        "accepted report payload hash does not verify"
    );
    anyhow::ensure!(
        report.status == AbRunStatus::Passed && report.passed,
        "only passed A/B reports may be imported"
    );
    anyhow::ensure!(
        !report.cap_expired && report.unstarted_workloads.is_empty(),
        "accepted report must finish every selected workload before its cap"
    );
    let config = profile.config();
    let expected_workload_ids = ab_profile_workloads(profile, &[])?;
    let expected_workloads = expected_workload_ids
        .iter()
        .map(|workload| workload.name().to_string())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        report.selected_workloads == expected_workloads,
        "accepted {} report does not contain its canonical workload matrix",
        profile.name()
    );
    anyhow::ensure!(
        report.workloads.len() == expected_workloads.len(),
        "accepted report workload results are incomplete"
    );
    anyhow::ensure!(
        report.provenance.profile == "dev"
            && report.provenance.features.is_empty()
            && report.provenance.bootstrap_seed == AB_BOOTSTRAP_SEED
            && report.provenance.worker_stack_bytes == AB_WORKER_STACK_BYTES
            && report.provenance.warmups_per_cluster == config.warmups
            && report.provenance.samples_per_cluster == config.max_pairs_per_cluster()
            && report.provenance.clusters == config.clusters
            && report.provenance.sequential_looks_per_cluster.as_slice() == config.looks
            && report.provenance.time_cap_seconds == config.cap.as_secs()
            && report.provenance.profile_configuration_sha256
                == ab_profile_configuration_hash(config, &expected_workload_ids)
            && report.provenance.fixture_sha256
                == ab_matrix_hash(&expected_workload_ids, ab_fixture_hash)
            && report.provenance.workload_schema_sha256
                == ab_matrix_hash(&expected_workload_ids, ab_workload_schema_hash)
            && report.provenance.workload_schema_version == AB_WORKLOAD_SCHEMA_VERSION
            && report.provenance.filtered_tree_identity_version
                == AB_FILTERED_TREE_IDENTITY_VERSION
            && report.provenance.metric_gate_version == AB_METRIC_GATE_VERSION,
        "accepted report profile or schema provenance does not match the current benchmark contract"
    );
    anyhow::ensure!(
        report.provenance.baseline_commit != report.provenance.candidate_commit
            && report.provenance.baseline_filtered_tree
                != report.provenance.candidate_filtered_tree,
        "accepted report does not identify distinct A/B revisions"
    );
    let requested_workloads = report
        .requested_workloads
        .iter()
        .map(|workload| AbWorkload::parse(workload))
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(
        ab_profile_workloads(profile, &requested_workloads)? == expected_workload_ids,
        "accepted report requested-workload selection is inconsistent"
    );
    anyhow::ensure!(
        report.replay_candidate_contention_self_test.is_none(),
        "accepted batch or final report contains replay-only evidence"
    );
    for (workload_report, workload) in report
        .workloads
        .iter_mut()
        .zip(expected_workload_ids.iter().copied())
    {
        validate_accepted_ab_workload(workload_report, workload, config)?;
    }
    let file_sha256 = sha256_bytes(&bytes);
    let destination = args
        .repo
        .join("docs")
        .join("benchmarks")
        .join("turn-latency")
        .join("accepted")
        .join(format!("{}-{report_payload_sha256}.json", profile.name()));
    install_accepted_ab_report(&destination, &bytes)?;
    Ok(AbImportReportReceipt {
        source: args.report.clone(),
        destination,
        execution_profile: profile,
        report_payload_sha256,
        file_sha256,
    })
}

fn install_accepted_ab_report(destination: &Path, bytes: &[u8]) -> Result<()> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    if destination.exists() {
        anyhow::ensure!(
            fs::read(destination)? == bytes,
            "accepted report destination already contains different bytes"
        );
        return Ok(());
    }

    let mut file = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "create adjacent accepted report for {}",
            destination.display()
        )
    })?;
    file.write_all(bytes)?;
    file.flush()?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(destination) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::ensure!(
                fs::read(destination)? == bytes,
                "accepted report destination already contains different bytes"
            );
            Ok(())
        }
        Err(error) => Err(error.error).with_context(|| {
            format!(
                "atomically install accepted report {}",
                destination.display()
            )
        }),
    }
}
