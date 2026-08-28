// A/B command, workload, sample, report, and provenance contracts.
//
// This file is included into the parent benchmark module so its private,
// benchmark-only contracts remain unchanged.

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AbWorkloadClass {
    #[default]
    Latency,
    CorrectnessOnly,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AbExecutionProfile {
    Quick,
    Batch,
    Final,
    Replay,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AbLatencyGateMode {
    Advisory,
    Hard,
    Excluded,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AbRunStatus {
    Passed,
    Failed,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AbSequentialDecision {
    Continue,
    Passed,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AbStopReason {
    AdvisoryComplete,
    CorrectnessOnlyComplete,
    CorrectnessFailure,
    LatencyClearPass,
    LatencyClearFailure,
    LatencyInvalid,
    LatencyUncertain,
    MaximumLookWithoutPass,
    ProfileTimeCap,
}

#[derive(Clone, Copy, Debug)]
struct AbExecutionConfig {
    profile: AbExecutionProfile,
    warmups: usize,
    clusters: usize,
    looks: &'static [usize],
    cap: Duration,
    latency_hard_gate: bool,
}

impl AbExecutionProfile {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "quick" => Ok(Self::Quick),
            "batch" => Ok(Self::Batch),
            "final" => Ok(Self::Final),
            "replay" => Ok(Self::Replay),
            _ => anyhow::bail!("unknown A/B execution profile `{value}`"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Batch => "batch",
            Self::Final => "final",
            Self::Replay => "replay",
        }
    }

    fn config(self) -> AbExecutionConfig {
        match self {
            Self::Quick => AbExecutionConfig {
                profile: self,
                warmups: 1,
                clusters: 2,
                looks: &AB_QUICK_LOOKS,
                cap: Duration::from_secs(5 * 60),
                latency_hard_gate: false,
            },
            Self::Batch => AbExecutionConfig {
                profile: self,
                warmups: 2,
                clusters: 2,
                looks: &AB_BATCH_LOOKS,
                cap: Duration::from_secs(10 * 60),
                latency_hard_gate: false,
            },
            Self::Final => AbExecutionConfig {
                profile: self,
                warmups: AB_WARMUPS,
                clusters: AB_CLUSTERS,
                looks: &AB_FINAL_LOOKS,
                cap: Duration::from_secs(30 * 60),
                latency_hard_gate: true,
            },
            Self::Replay => AbExecutionConfig {
                profile: self,
                warmups: 0,
                clusters: 1,
                looks: &AB_REPLAY_LOOKS,
                cap: Duration::from_secs(10 * 60),
                latency_hard_gate: true,
            },
        }
    }
}

impl AbExecutionConfig {
    fn max_pairs_per_cluster(self) -> usize {
        let Some(max_pairs) = self.looks.last().copied() else {
            unreachable!("A/B execution profiles always declare a look");
        };
        max_pairs
    }

    fn ucb_quantile(self) -> f64 {
        if self.profile == AbExecutionProfile::Final {
            1.0 - AB_FAMILY_WISE_ALPHA / self.looks.len() as f64
        } else {
            1.0 - AB_FAMILY_WISE_ALPHA
        }
    }

    fn lcb_quantile(self) -> f64 {
        1.0 - self.ucb_quantile()
    }

    fn latency_gate_mode(self, workload_class: AbWorkloadClass) -> AbLatencyGateMode {
        if workload_class == AbWorkloadClass::CorrectnessOnly {
            AbLatencyGateMode::Excluded
        } else if self.latency_hard_gate {
            AbLatencyGateMode::Hard
        } else {
            AbLatencyGateMode::Advisory
        }
    }

    fn looks_for(self, workload: AbWorkload) -> &'static [usize] {
        if workload.class() == AbWorkloadClass::CorrectnessOnly {
            &AB_CORRECTNESS_ONLY_LOOKS
        } else {
            self.looks
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AbWorkload {
    #[default]
    CodeModeNestedDispatch,
    LongHistoryNoToolInitial,
    LongHistoryToolContinuation,
    StableContextWarmCache,
    ContextChangeInvalidation,
    SingleDirectToolCall,
    ParallelSafeTripleDirect,
    ExclusiveGateSerialization,
    CodeModeHighVolume,
    RetainedExecWriteStdinLifecycle,
    AbortDirectNestedInFlight,
    AbortRetainedProcess,
    SessionReplay,
}

impl AbWorkload {
    const ALL: [Self; 12] = [
        Self::LongHistoryNoToolInitial,
        Self::LongHistoryToolContinuation,
        Self::StableContextWarmCache,
        Self::ContextChangeInvalidation,
        Self::SingleDirectToolCall,
        Self::ParallelSafeTripleDirect,
        Self::ExclusiveGateSerialization,
        Self::CodeModeHighVolume,
        Self::RetainedExecWriteStdinLifecycle,
        Self::AbortDirectNestedInFlight,
        Self::AbortRetainedProcess,
        Self::SessionReplay,
    ];
    const MATRIX: [Self; 11] = [
        Self::LongHistoryNoToolInitial,
        Self::LongHistoryToolContinuation,
        Self::StableContextWarmCache,
        Self::ContextChangeInvalidation,
        Self::SingleDirectToolCall,
        Self::ParallelSafeTripleDirect,
        Self::ExclusiveGateSerialization,
        Self::CodeModeHighVolume,
        Self::RetainedExecWriteStdinLifecycle,
        Self::AbortDirectNestedInFlight,
        Self::AbortRetainedProcess,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::CodeModeNestedDispatch => "code_mode_nested_dispatch",
            Self::LongHistoryNoToolInitial => "long_history_no_tool_initial",
            Self::LongHistoryToolContinuation => "long_history_tool_continuation",
            Self::StableContextWarmCache => "stable_context_warm_cache",
            Self::ContextChangeInvalidation => "context_change_invalidation",
            Self::SingleDirectToolCall => "single_direct_tool_call",
            Self::ParallelSafeTripleDirect => "parallel_safe_triple_direct",
            Self::ExclusiveGateSerialization => "exclusive_gate_serialization",
            Self::CodeModeHighVolume => "code_mode_high_volume",
            Self::RetainedExecWriteStdinLifecycle => "retained_exec_write_stdin_lifecycle",
            Self::AbortDirectNestedInFlight => "abort_direct_nested_in_flight",
            Self::AbortRetainedProcess => "abort_retained_process",
            Self::SessionReplay => "session_replay",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        anyhow::ensure!(!value.is_empty(), "A/B workload selection cannot be empty");
        if value == Self::SessionReplay.name() {
            return Ok(Self::SessionReplay);
        }
        Self::MATRIX
            .into_iter()
            .find(|workload| workload.name() == value)
            .with_context(|| format!("unknown A/B workload `{value}`"))
    }

    fn class(self) -> AbWorkloadClass {
        match self {
            Self::ParallelSafeTripleDirect
            | Self::CodeModeHighVolume
            | Self::AbortDirectNestedInFlight
            | Self::AbortRetainedProcess => AbWorkloadClass::CorrectnessOnly,
            Self::SessionReplay => AbWorkloadClass::Latency,
            _ => AbWorkloadClass::Latency,
        }
    }

    fn allows_raw_baseline_behavior(self) -> bool {
        matches!(
            self,
            Self::AbortDirectNestedInFlight | Self::AbortRetainedProcess
        )
    }

    fn is_request_cache(self) -> bool {
        matches!(
            self,
            Self::LongHistoryNoToolInitial
                | Self::LongHistoryToolContinuation
                | Self::StableContextWarmCache
                | Self::ContextChangeInvalidation
        )
    }

    fn restores_missing_baseline_tool_outputs(self) -> bool {
        matches!(
            self,
            Self::ParallelSafeTripleDirect | Self::ExclusiveGateSerialization
        )
    }

    fn expected_logical_generations(self) -> u32 {
        match self {
            Self::CodeModeNestedDispatch => 2,
            Self::LongHistoryToolContinuation => 2,
            Self::LongHistoryNoToolInitial
            | Self::StableContextWarmCache
            | Self::ContextChangeInvalidation => 1,
            Self::SingleDirectToolCall
            | Self::ParallelSafeTripleDirect
            | Self::ExclusiveGateSerialization => 2,
            Self::CodeModeHighVolume => (AB_HIGH_VOLUME_SUBTURNS * 2) as u32,
            Self::RetainedExecWriteStdinLifecycle => 4,
            Self::AbortDirectNestedInFlight => 1,
            Self::AbortRetainedProcess => 1,
            Self::SessionReplay => AB_REPLAY_B_GENERATIONS,
        }
    }

    fn expected_direct_tool_calls(self) -> u32 {
        match self {
            Self::CodeModeNestedDispatch => 1,
            Self::LongHistoryToolContinuation => 1,
            Self::LongHistoryNoToolInitial
            | Self::StableContextWarmCache
            | Self::ContextChangeInvalidation => 0,
            Self::SingleDirectToolCall => 1,
            Self::ParallelSafeTripleDirect | Self::ExclusiveGateSerialization => 3,
            Self::CodeModeHighVolume => {
                (AB_HIGH_VOLUME_SUBTURNS * AB_HIGH_VOLUME_DIRECT_CALLS_PER_GENERATION) as u32
            }
            Self::RetainedExecWriteStdinLifecycle => 3,
            Self::AbortDirectNestedInFlight => 1,
            Self::AbortRetainedProcess => 1,
            Self::SessionReplay => 0,
        }
    }

    fn expected_nested_tool_calls(self) -> u32 {
        match self {
            Self::CodeModeNestedDispatch => 1,
            Self::LongHistoryNoToolInitial
            | Self::LongHistoryToolContinuation
            | Self::StableContextWarmCache
            | Self::ContextChangeInvalidation
            | Self::SingleDirectToolCall
            | Self::ParallelSafeTripleDirect
            | Self::ExclusiveGateSerialization => 0,
            Self::CodeModeHighVolume => {
                (AB_HIGH_VOLUME_SUBTURNS * AB_HIGH_VOLUME_NESTED_CALLS_PER_GENERATION) as u32
            }
            Self::RetainedExecWriteStdinLifecycle => 0,
            Self::AbortDirectNestedInFlight => 1,
            Self::AbortRetainedProcess => 0,
            Self::SessionReplay => 0,
        }
    }

    fn latency_metrics(self) -> &'static [AbLatencyMetric] {
        match self {
            Self::CodeModeNestedDispatch => &AbLatencyMetric::ALL,
            Self::LongHistoryNoToolInitial
            | Self::StableContextWarmCache
            | Self::ContextChangeInvalidation => &AbLatencyMetric::REQUEST_ONLY,
            Self::LongHistoryToolContinuation => &AbLatencyMetric::ALL,
            Self::SingleDirectToolCall => &AbLatencyMetric::ALL,
            // Raw A omits the tool outputs that the overlay restores for B. Keep
            // the exact concurrency/graph gates, but do not bootstrap unequal work.
            Self::ParallelSafeTripleDirect => &[],
            Self::ExclusiveGateSerialization => &AbLatencyMetric::WITH_PARALLEL_GATE_WAIT,
            Self::CodeModeHighVolume => &[],
            Self::RetainedExecWriteStdinLifecycle => &AbLatencyMetric::ALL,
            Self::AbortDirectNestedInFlight | Self::AbortRetainedProcess => &[],
            Self::SessionReplay => &AbLatencyMetric::REPLAY,
        }
    }

    fn report_shape(self) -> AbWorkloadShape {
        match self {
            Self::CodeModeNestedDispatch => AbWorkloadShape {
                aggregation: "one successful tool turn plus its completion generation".to_string(),
                subturns_per_sample: 1,
                logical_generations_per_sample: 2,
                direct_outer_calls_per_generation: 1,
                nested_calls_per_generation: 1,
                nested_calls_by_outer_call: vec![1],
                direct_outer_calls_per_sample: 1,
                nested_calls_per_sample: 1,
                subturn_terminal_outcome: "successful_continuation".to_string(),
                model_requests_per_sample: 2,
                history_seed_turns: 0,
                cache_assertion: "none".to_string(),
                latency_metrics: self
                    .latency_metrics()
                    .iter()
                    .map(|metric| metric.name().to_string())
                    .collect(),
            },
            Self::LongHistoryNoToolInitial => AbWorkloadShape {
                aggregation: "one no-tool turn over a fixed thirty-two-turn model-visible history"
                    .to_string(),
                subturns_per_sample: 1,
                logical_generations_per_sample: 1,
                direct_outer_calls_per_generation: 0,
                nested_calls_per_generation: 0,
                nested_calls_by_outer_call: Vec::new(),
                direct_outer_calls_per_sample: 0,
                nested_calls_per_sample: 0,
                subturn_terminal_outcome: "successful_no_tool_completion".to_string(),
                model_requests_per_sample: 1,
                history_seed_turns: AB_LONG_HISTORY_TURNS as u32,
                cache_assertion: "raw request components captured".to_string(),
                latency_metrics: self
                    .latency_metrics()
                    .iter()
                    .map(|metric| metric.name().to_string())
                    .collect(),
            },
            Self::LongHistoryToolContinuation => AbWorkloadShape {
                aggregation:
                    "one successful direct-tool turn and continuation over fixed long history"
                        .to_string(),
                subturns_per_sample: 1,
                logical_generations_per_sample: 2,
                direct_outer_calls_per_generation: 1,
                nested_calls_per_generation: 0,
                nested_calls_by_outer_call: vec![0],
                direct_outer_calls_per_sample: 1,
                nested_calls_per_sample: 0,
                subturn_terminal_outcome: "successful_tool_continuation".to_string(),
                model_requests_per_sample: 2,
                history_seed_turns: AB_LONG_HISTORY_TURNS as u32,
                cache_assertion: "continuation preserves stable request scaffolding".to_string(),
                latency_metrics: self
                    .latency_metrics()
                    .iter()
                    .map(|metric| metric.name().to_string())
                    .collect(),
            },
            Self::StableContextWarmCache => AbWorkloadShape {
                aggregation: "one rollback-isolated repetition after profile-declared same-context cache warmups"
                    .to_string(),
                subturns_per_sample: 1,
                logical_generations_per_sample: 1,
                direct_outer_calls_per_generation: 0,
                nested_calls_per_generation: 0,
                nested_calls_by_outer_call: Vec::new(),
                direct_outer_calls_per_sample: 0,
                nested_calls_per_sample: 0,
                subturn_terminal_outcome: "successful_warm_cache_completion".to_string(),
                model_requests_per_sample: 1,
                history_seed_turns: AB_LONG_HISTORY_TURNS as u32,
                cache_assertion: "all five declared request components reused".to_string(),
                latency_metrics: self
                    .latency_metrics()
                    .iter()
                    .map(|metric| metric.name().to_string())
                    .collect(),
            },
            Self::ContextChangeInvalidation => AbWorkloadShape {
                aggregation:
                    "one rollback-isolated alternating-input request after cache warmups"
                        .to_string(),
                subturns_per_sample: 1,
                logical_generations_per_sample: 1,
                direct_outer_calls_per_generation: 0,
                nested_calls_per_generation: 0,
                nested_calls_by_outer_call: Vec::new(),
                direct_outer_calls_per_sample: 0,
                nested_calls_per_sample: 0,
                subturn_terminal_outcome: "successful_context_change_completion".to_string(),
                model_requests_per_sample: 1,
                history_seed_turns: AB_LONG_HISTORY_TURNS as u32,
                cache_assertion:
                    "only current_input changes; instructions, schemas, history, and cache key reuse"
                        .to_string(),
                latency_metrics: self
                    .latency_metrics()
                    .iter()
                    .map(|metric| metric.name().to_string())
                    .collect(),
            },
            Self::SingleDirectToolCall => AbWorkloadShape {
                aggregation: "one direct test_sync_tool call plus its completion generation"
                    .to_string(),
                subturns_per_sample: 1,
                logical_generations_per_sample: 2,
                direct_outer_calls_per_generation: 1,
                nested_calls_per_generation: 0,
                nested_calls_by_outer_call: vec![0],
                direct_outer_calls_per_sample: 1,
                nested_calls_per_sample: 0,
                subturn_terminal_outcome: "successful_single_direct_continuation".to_string(),
                model_requests_per_sample: 2,
                history_seed_turns: 0,
                cache_assertion: "none; one direct call has no parallel-gate wait".to_string(),
                latency_metrics: self
                    .latency_metrics()
                    .iter()
                    .map(|metric| metric.name().to_string())
                    .collect(),
            },
            Self::ParallelSafeTripleDirect => AbWorkloadShape {
                aggregation:
                    "one generation emitting exactly three barrier-synchronized parallel-safe direct calls plus completion"
                        .to_string(),
                subturns_per_sample: 1,
                logical_generations_per_sample: 2,
                direct_outer_calls_per_generation: 3,
                nested_calls_per_generation: 0,
                nested_calls_by_outer_call: vec![0, 0, 0],
                direct_outer_calls_per_sample: 3,
                nested_calls_per_sample: 0,
                subturn_terminal_outcome: "successful_parallel_safe_continuation".to_string(),
                model_requests_per_sample: 2,
                history_seed_turns: 0,
                cache_assertion:
                    "none; three calls share one generation and reach concurrency three without gate wait"
                        .to_string(),
                latency_metrics: self
                    .latency_metrics()
                    .iter()
                    .map(|metric| metric.name().to_string())
                    .collect(),
            },
            Self::ExclusiveGateSerialization => AbWorkloadShape {
                aggregation:
                    "one generation emitting two same-workspace exec_command calls and one unrelated parallel-safe call plus completion"
                        .to_string(),
                subturns_per_sample: 1,
                logical_generations_per_sample: 2,
                direct_outer_calls_per_generation: 3,
                nested_calls_per_generation: 0,
                nested_calls_by_outer_call: vec![0, 0, 0],
                direct_outer_calls_per_sample: 3,
                nested_calls_per_sample: 0,
                subturn_terminal_outcome: "successful_exclusive_serialized_continuation"
                    .to_string(),
                model_requests_per_sample: 2,
                history_seed_turns: 0,
                cache_assertion:
                    "none; same-resource exec calls serialize while the unrelated safe call overlaps without gate wait or convoy"
                        .to_string(),
                latency_metrics: self
                    .latency_metrics()
                    .iter()
                    .map(|metric| metric.name().to_string())
                    .collect(),
            },
            Self::CodeModeHighVolume => AbWorkloadShape {
                aggregation: "sixteen deterministic tool-generation subturns per sample, each followed by one completion generation"
                    .to_string(),
                subturns_per_sample: AB_HIGH_VOLUME_SUBTURNS as u32,
                logical_generations_per_sample: (AB_HIGH_VOLUME_SUBTURNS * 2) as u32,
                direct_outer_calls_per_generation: AB_HIGH_VOLUME_DIRECT_CALLS_PER_GENERATION
                    as u32,
                nested_calls_per_generation: AB_HIGH_VOLUME_NESTED_CALLS_PER_GENERATION as u32,
                nested_calls_by_outer_call: vec![1, 2],
                direct_outer_calls_per_sample: (AB_HIGH_VOLUME_SUBTURNS
                    * AB_HIGH_VOLUME_DIRECT_CALLS_PER_GENERATION)
                    as u32,
                nested_calls_per_sample: (AB_HIGH_VOLUME_SUBTURNS
                    * AB_HIGH_VOLUME_NESTED_CALLS_PER_GENERATION)
                    as u32,
                subturn_terminal_outcome: "successful_high_volume_continuation".to_string(),
                model_requests_per_sample: (AB_HIGH_VOLUME_SUBTURNS * 2) as u32,
                history_seed_turns: 0,
                cache_assertion: "none".to_string(),
                latency_metrics: self
                    .latency_metrics()
                    .iter()
                    .map(|metric| metric.name().to_string())
                    .collect(),
            },
            Self::RetainedExecWriteStdinLifecycle => AbWorkloadShape {
                aggregation: "one retained exec_command, exactly two correlated write_stdin polls, final exit, and cleanup"
                    .to_string(),
                subturns_per_sample: 1,
                logical_generations_per_sample: 4,
                direct_outer_calls_per_generation: 1,
                nested_calls_per_generation: 0,
                nested_calls_by_outer_call: vec![0, 0, 0],
                direct_outer_calls_per_sample: 3,
                nested_calls_per_sample: 0,
                subturn_terminal_outcome: "passed_after_retained_process_exit_and_cleanup"
                    .to_string(),
                model_requests_per_sample: 4,
                history_seed_turns: 0,
                cache_assertion: "none; one durable process identity spans both polls".to_string(),
                latency_metrics: self
                    .latency_metrics()
                    .iter()
                    .map(|metric| metric.name().to_string())
                    .collect(),
            },
            Self::AbortDirectNestedInFlight => AbWorkloadShape {
                aggregation: "one interrupted CodeMode generation after both the direct exec and nested permission call are accepted"
                    .to_string(),
                subturns_per_sample: 1,
                logical_generations_per_sample: 1,
                direct_outer_calls_per_generation: 1,
                nested_calls_per_generation: 1,
                nested_calls_by_outer_call: vec![1],
                direct_outer_calls_per_sample: 1,
                nested_calls_per_sample: 1,
                subturn_terminal_outcome:
                    "turn_aborted_after_ordered_direct_nested_closure".to_string(),
                model_requests_per_sample: 1,
                history_seed_turns: 0,
                cache_assertion: "none; permission request is the in-flight interrupt barrier"
                    .to_string(),
                latency_metrics: Vec::new(),
            },
            Self::AbortRetainedProcess => AbWorkloadShape {
                aggregation: "one interrupted retained exec_command after durable process ownership is observed"
                    .to_string(),
                subturns_per_sample: 1,
                logical_generations_per_sample: 1,
                direct_outer_calls_per_generation: 1,
                nested_calls_per_generation: 0,
                nested_calls_by_outer_call: vec![0],
                direct_outer_calls_per_sample: 1,
                nested_calls_per_sample: 0,
                subturn_terminal_outcome:
                    "turn_aborted_after_exact_result_persistence_and_process_cleanup".to_string(),
                model_requests_per_sample: 1,
                history_seed_turns: 0,
                cache_assertion:
                    "none; retained process ownership must precede interrupt and cleanup"
                        .to_string(),
                latency_metrics: Vec::new(),
            },
            Self::SessionReplay => AbWorkloadShape {
                aggregation: "three linked subturns: actionable success, recoverable exec failure with artifact repair, and retained-process abort".to_string(),
                subturns_per_sample: 3,
                logical_generations_per_sample: AB_REPLAY_B_GENERATIONS,
                direct_outer_calls_per_generation: 0,
                nested_calls_per_generation: 0,
                nested_calls_by_outer_call: vec![2, 3],
                direct_outer_calls_per_sample: 0,
                nested_calls_per_sample: 0,
                subturn_terminal_outcome: "passed,failed,aborted_after_closure".to_string(),
                model_requests_per_sample: AB_REPLAY_B_GENERATIONS,
                history_seed_turns: AB_LONG_HISTORY_TURNS as u32,
                cache_assertion: "one reused worker per revision with verified rollback reset before every pair".to_string(),
                latency_metrics: AbLatencyMetric::REPLAY
                    .iter()
                    .map(|metric| metric.name().to_string())
                    .collect(),
            },
        }
    }
}

fn ab_controller_workloads() -> &'static [AbWorkload] {
    &AbWorkload::MATRIX
}

fn ab_all_workloads() -> &'static [AbWorkload] {
    &AbWorkload::ALL
}

fn ab_profile_workloads(
    profile: AbExecutionProfile,
    requested: &[AbWorkload],
) -> Result<Vec<AbWorkload>> {
    let mut seen = BTreeSet::new();
    for workload in requested {
        anyhow::ensure!(
            seen.insert(workload.name()),
            "duplicate A/B workload selection `{}`",
            workload.name()
        );
    }
    if profile == AbExecutionProfile::Replay {
        anyhow::ensure!(
            requested.is_empty(),
            "replay A/B profile rejects --workload selection and always runs session_replay"
        );
        return Ok(vec![AbWorkload::SessionReplay]);
    }
    anyhow::ensure!(
        !requested.contains(&AbWorkload::SessionReplay),
        "session_replay is selected only by the replay profile"
    );
    if requested.is_empty() {
        return Ok(ab_controller_workloads().to_vec());
    }
    Ok(ab_controller_workloads()
        .iter()
        .copied()
        .filter(|workload| seen.contains(workload.name()))
        .collect())
}

#[derive(Debug)]
struct Args {
    scenario: Option<Scenario>,
    mode: Option<Mode>,
    iterations: usize,
    warmups: usize,
    clusters: usize,
    absolute_margin_ms: f64,
    relative_margin: f64,
}

#[derive(Debug)]
enum BenchmarkCommand {
    CodeModeCapture { host: PathBuf },
    AbCapture(AbCaptureArgs),
    AbPrepare(AbPrepareArgs),
    AbCompare(AbCompareArgs),
    AbImportReport(AbImportReportArgs),
    AbWorker(AbWorkerArgs),
    AbExclusiveGateChild,
    AbRetainedChild,
    AbReplayCommand { mode: String, paths: Vec<PathBuf> },
    Synthetic(Args),
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AbReplaySubturnRecord {
    name: String,
    logical_generations: u32,
    terminal_event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion_status: Option<String>,
    application_result: String,
    typed_error_count: u32,
    final_response_present: bool,
    closure_complete: bool,
    #[serde(default)]
    follow_up_artifact_present: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AbReplayTargetedActionEvidence {
    action_first_instruction_observed: bool,
    generation_index: u32,
    action: String,
    exact_target: String,
    targeted: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AbReplayResetProof {
    before_sha256: String,
    after_sha256: String,
    passed: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Sample {
    duration_ns: u64,
    inclusive_duration_ns: u64,
    machine_duration_ns: u64,
    controllable_duration_ns: u64,
    model_wait_ns: u64,
    #[serde(default)]
    model_request_wait_ns: u64,
    #[serde(default)]
    model_stream_processing_ns: u64,
    tool_active_ns: u64,
    orchestration_ns: u64,
    standalone_work_ns: u64,
    finalization_ns: u64,
    preparation_ns: u64,
    #[serde(default)]
    planning_ns: u64,
    #[serde(default)]
    router_build_ns: u64,
    #[serde(default)]
    persistence_union_ns: u64,
    #[serde(default)]
    startup_prewarm_wait_ns: u64,
    pre_first_output_ns: u64,
    #[serde(default)]
    first_request_dispatch_ready_ns: u64,
    #[serde(default)]
    pre_first_client_critical_path_ns: u64,
    #[serde(default)]
    pre_first_attributed_client_union_ns: u64,
    #[serde(default)]
    pre_first_unattributed_ns: u64,
    #[serde(default)]
    history_snapshot_ns: u64,
    #[serde(default)]
    normalization_ns: u64,
    #[serde(default)]
    prompt_construction_ns: u64,
    #[serde(default)]
    request_transformation_ns: u64,
    #[serde(default)]
    serialization_ns: u64,
    #[serde(default)]
    transport_readiness_ns: u64,
    sampling_to_call_ns: u64,
    post_tool_handoff_ns: u64,
    parallel_gate_wait_ns: u64,
    parallel_gate_wait_max_ns: u64,
    #[serde(default)]
    parallel_gate_waiter_depth_max: u32,
    max_concurrent_tool_calls: u32,
    convoy_count: u32,
    #[serde(default)]
    unrelated_parallel_safe_convoy_count: u32,
    workspace_evidence_before_ns: u64,
    workspace_evidence_after_ns: u64,
    workspace_evidence_cache_hits: u32,
    workspace_evidence_fresh_captures: u32,
    workspace_evidence_timeouts: u32,
    logical_generations: u32,
    provider_attempts: u32,
    retry_attempts: u32,
    fallback_attempts: u32,
    avoidable_generations: u32,
    provider_input_tokens: u64,
    provider_cached_input_tokens: u64,
    provider_visible_output_tokens: u64,
    provider_reasoning_tokens: u64,
    provider_total_tokens: u64,
    token_usage_records: u32,
    missing_token_usage_records: u32,
    prompt_instruction_tokens: u64,
    prompt_schema_tokens: u64,
    prompt_history_tokens: u64,
    prompt_current_input_tokens: u64,
    prompt_repository_tokens: u64,
    prompt_skill_tokens: u64,
    prompt_injected_tokens: u64,
    prompt_reconciliation_residual: i64,
    repeated_unchanged_context_tokens: u64,
    between_tools_peak_input_tokens: u64,
    nonprogress_tokens: u64,
    nonprogress_latency_ns: u64,
    repeated_waits: u32,
    #[serde(default)]
    tool_router_reuse_count: u32,
    #[serde(default)]
    tool_router_rebuild_count: u32,
    direct_tool_calls: u32,
    nested_tool_calls: u32,
    paired_tool_calls: u32,
    unresolved_tool_calls: u32,
    orphan_tool_calls: u32,
    #[serde(default)]
    workload_subturns: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    replay_subturns: Vec<AbReplaySubturnRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replay_targeted_action: Option<AbReplayTargetedActionEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replay_reset: Option<AbReplayResetProof>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    generation_purposes: BTreeMap<String, u32>,
    #[serde(default)]
    failure_terminalized_subturns: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_call_graph: Vec<AbToolGraphCallCompat>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_gate_calls: Vec<AbToolGateCallCompat>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    request_components: Vec<AbRequestComponentSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    canonical_request_body_sha256: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_component_delta: Option<AbRequestComponentDelta>,
    #[serde(default)]
    history_seed_turns_visible: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_closure: Option<AbToolClosureCompat>,
    #[serde(default)]
    terminal_event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    abort_reason: Option<String>,
    #[serde(default)]
    typed_error_count: u32,
    #[serde(default)]
    final_response_present: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    abort_registered_call_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    abort_terminal_outcomes_by_registration: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    abort_barrier_call_id: Option<String>,
    #[serde(default)]
    abort_model_resumed_call_count: u32,
    #[serde(default)]
    forged_turn_complete_observed: bool,
    #[serde(default)]
    retained_write_stdin_poll_count: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    retained_session_ids: Vec<String>,
    #[serde(default)]
    retained_process_exit_observed: bool,
    #[serde(default)]
    retained_process_cleanup_complete: bool,
    #[serde(default)]
    retained_process_owned_before_abort: bool,
    #[serde(default)]
    retained_process_count_before_abort: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retained_abort_process_id: Option<String>,
    #[serde(default)]
    retained_abort_persisted_result_count: u32,
    #[serde(default)]
    retained_abort_cancellation_observed: bool,
    incomplete_lifecycle_calls: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    incomplete_tool_lifecycles: Vec<AbIncompleteToolLifecycle>,
    unexpected_live_processes: u32,
    expected_retained_processes: u32,
    #[serde(default)]
    output_projection_count: u32,
    #[serde(default)]
    output_truncation_count: u32,
    #[serde(default)]
    output_projected_token_count: u64,
    #[serde(default)]
    output_canonical_byte_count: u64,
    #[serde(default)]
    output_canonical_token_count: u64,
    #[serde(default)]
    output_model_byte_count: u64,
    #[serde(default)]
    output_model_token_count: u64,
    #[serde(default)]
    output_artifact_creation_count: u32,
    #[serde(default)]
    output_artifact_reuse_count: u32,
    #[serde(default)]
    output_artifact_reread_count: u32,
    #[serde(default)]
    output_projection_truncation_count: u32,
    #[serde(default)]
    output_omitted_section_count: u64,
    #[serde(default)]
    output_recovery_count: u32,
    #[serde(default)]
    output_recovery_retruncation_count: u32,
    #[serde(default)]
    output_recursive_spill_count: u32,
    timing_overflow_count: u32,
    timing_anomaly_count: u32,
    unclassified_ns: u64,
    timing_profile_valid: bool,
    classification_complete: bool,
    lifecycle_complete: bool,
    token_coverage_complete: bool,
    decision_coverage_complete: bool,
    latency_eligible: bool,
    sampling_requests: u32,
    failed: bool,
    failure_codes: Vec<String>,
    serialized_bytes: u64,
    cache_hits: u32,
    exec_description_tokens: u64,
    prompt_input_tokens: u64,
    tool_calls: u32,
    max_ready_to_sample_to_dispatch_ns: Option<u64>,
}

#[derive(Clone, Debug)]
struct AbCaptureArgs {
    repo: PathBuf,
    state: PathBuf,
}

#[derive(Clone, Debug)]
struct AbPrepareArgs {
    state: PathBuf,
    candidate_repo: PathBuf,
    work_root: PathBuf,
    manifest: PathBuf,
    baseline_target_dir: Option<PathBuf>,
    candidate_target_dir: Option<PathBuf>,
    reuse_work_root: bool,
}

#[derive(Clone, Debug)]
struct AbCompareArgs {
    manifest: PathBuf,
    report: PathBuf,
    profile: AbExecutionProfile,
    requested_workloads: Vec<AbWorkload>,
}

#[derive(Clone, Debug)]
struct AbImportReportArgs {
    report: PathBuf,
    repo: PathBuf,
}

#[derive(Debug, Serialize)]
struct AbImportReportReceipt {
    source: PathBuf,
    destination: PathBuf,
    execution_profile: AbExecutionProfile,
    report_payload_sha256: String,
    file_sha256: String,
}

#[derive(Clone, Debug)]
struct AbWorkerArgs {
    code_mode_host: PathBuf,
    variant: String,
    cluster: usize,
    workload: AbWorkload,
    warmups: usize,
    samples: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AbBaselineState {
    schema_version: u16,
    #[serde(default)]
    filtered_tree_identity_version: u16,
    repository: PathBuf,
    baseline_commit: String,
    baseline_filtered_tree: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AbPreparedBuild {
    worktree: PathBuf,
    target_dir: PathBuf,
    cli: PathBuf,
    host: PathBuf,
    worker: PathBuf,
    cli_sha256: String,
    host_sha256: String,
    worker_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AbPreparedManifest {
    schema_version: u16,
    baseline_commit: String,
    candidate_commit: String,
    baseline_filtered_tree: String,
    candidate_filtered_tree: String,
    overlay_sha256: String,
    fixture_matrix_sha256: String,
    workload_schema_matrix_sha256: String,
    build_configuration_sha256: String,
    rustc_version: String,
    rust_target: String,
    baseline: AbPreparedBuild,
    candidate: AbPreparedBuild,
    manifest_payload_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AbToolCallIdentityCompat {
    call_id: String,
    execution_id: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    parent_call_id: Option<String>,
    #[serde(default)]
    sampling_generation_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AbToolClosureCompat {
    accepted_count: u32,
    timing_paired_count: u32,
    terminal_count: u32,
    persisted_count: u32,
    duplicate_call_id_count: u32,
    duplicate_acceptance_count: u32,
    duplicate_timing_count: u32,
    duplicate_persistence_count: u32,
    orphan_timing_count: u32,
    orphan_persistence_count: u32,
    overflow_count: u32,
    #[serde(default)]
    unresolved_calls: Vec<AbToolCallIdentityCompat>,
    #[serde(default)]
    orphan_calls: Vec<AbToolCallIdentityCompat>,
    complete: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AbToolGraphCallCompat {
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    execution_id: String,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    parent_call_id: Option<String>,
    #[serde(default)]
    sampling_generation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workload_generation_index: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AbToolGateCallCompat {
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
    #[serde(default)]
    parallel_gate_wait_ns: u64,
    #[serde(default)]
    parallel_gate_waiter_depth_max: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handler_entry_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handler_exit_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct AbIncompleteToolLifecycle {
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    tool_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    missing_boundaries: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    nonmonotonic_boundaries: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AbRequestComponentSnapshot {
    stage: String,
    envelope_sha256: String,
    instructions_sha256: String,
    tool_schemas_sha256: String,
    history_sha256: String,
    current_input_sha256: String,
    prompt_cache_key_sha256: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AbRequestComponentDelta {
    compared_to_previous: bool,
    changed_components: Vec<String>,
    reused_components: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct AbWorkloadShape {
    aggregation: String,
    subturns_per_sample: u32,
    logical_generations_per_sample: u32,
    direct_outer_calls_per_generation: u32,
    nested_calls_per_generation: u32,
    nested_calls_by_outer_call: Vec<u32>,
    direct_outer_calls_per_sample: u32,
    nested_calls_per_sample: u32,
    subturn_terminal_outcome: String,
    #[serde(default)]
    model_requests_per_sample: u32,
    #[serde(default)]
    history_seed_turns: u32,
    #[serde(default)]
    cache_assertion: String,
    #[serde(default)]
    latency_metrics: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AbWarmupFailure {
    warmup_index: usize,
    failure_codes: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AbWorkerReady {
    kind: String,
    variant: String,
    cluster: usize,
    #[serde(default)]
    workload: AbWorkload,
    warmups: usize,
    #[serde(default)]
    samples: usize,
    warmup_failures: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warmup_failure_details: Vec<AbWarmupFailure>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AbWorkerResponse {
    kind: String,
    pair_index: usize,
    sample: Sample,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AbWorkerCommand {
    kind: String,
    pair_index: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AbPairedCluster {
    cluster: usize,
    a_first: Vec<bool>,
    a_samples: Vec<Sample>,
    b_samples: Vec<Sample>,
    a_warmup_failures: usize,
    b_warmup_failures: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    a_warmup_failure_details: Vec<AbWarmupFailure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    b_warmup_failure_details: Vec<AbWarmupFailure>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AbLatencyGate {
    metric: String,
    point_median_ratio: f64,
    point_p95_ratio: f64,
    median_ratio_lcb: f64,
    p95_ratio_lcb: f64,
    median_ratio_ucb: f64,
    p95_ratio_ucb: f64,
    lcb_quantile: f64,
    ucb_quantile: f64,
    pairs_per_cluster: usize,
    median_ratio_ucb_limit: f64,
    p95_ratio_ucb_limit: f64,
    target_ratio: f64,
    passed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AbProvenance {
    baseline_commit: String,
    candidate_commit: String,
    baseline_filtered_tree: String,
    candidate_filtered_tree: String,
    overlay_sha256: String,
    prepared_manifest_sha256: String,
    fixture_sha256: String,
    workload_schema_sha256: String,
    baseline_worker_sha256: String,
    candidate_worker_sha256: String,
    baseline_host_binary_sha256: String,
    candidate_host_binary_sha256: String,
    baseline_cli_binary_sha256: String,
    candidate_cli_binary_sha256: String,
    rustc_version: String,
    rust_target: String,
    profile: String,
    execution_profile: AbExecutionProfile,
    features: Vec<String>,
    bootstrap_seed: u64,
    worker_stack_bytes: String,
    warmups_per_cluster: usize,
    samples_per_cluster: usize,
    clusters: usize,
    sequential_looks_per_cluster: Vec<usize>,
    time_cap_seconds: u64,
    profile_configuration_sha256: String,
    workload_schema_version: u16,
    filtered_tree_identity_version: u16,
    metric_gate_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replay_session_audit: Option<AbReplaySessionAuditEvidence>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct AbReplaySessionAuditEvidence {
    schema_version: u16,
    audited_turns: u32,
    active_seconds: u64,
    logical_generations: u32,
    orchestration_seconds: u64,
    model_seconds: u64,
    input_tokens: u64,
    input_tokens_approximate: bool,
    nonprogress_tokens: u64,
    nonprogress_tokens_approximate: bool,
    first_response_targeted_actions: u32,
}

fn replay_session_audit_evidence() -> AbReplaySessionAuditEvidence {
    AbReplaySessionAuditEvidence {
        schema_version: AB_REPLAY_SESSION_AUDIT_EVIDENCE_VERSION,
        audited_turns: 13,
        active_seconds: 1_457,
        logical_generations: 83,
        orchestration_seconds: 876,
        model_seconds: 549,
        input_tokens: 1_720_000,
        input_tokens_approximate: true,
        nonprogress_tokens: 260_000,
        nonprogress_tokens_approximate: true,
        first_response_targeted_actions: 0,
    }
}

fn validate_replay_session_audit_provenance(provenance: &AbProvenance) -> Result<()> {
    if provenance.execution_profile == AbExecutionProfile::Replay {
        anyhow::ensure!(
            provenance.replay_session_audit.as_ref() == Some(&replay_session_audit_evidence()),
            "accepted replay report has missing or invalid session-audit provenance"
        );
    } else {
        anyhow::ensure!(
            provenance.replay_session_audit.is_none(),
            "session-audit provenance is valid only for replay reports"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AbSequentialLook {
    pairs_per_cluster: usize,
    total_pairs: usize,
    ucb_quantile: f64,
    latency_gates: Vec<AbLatencyGate>,
    latency_diagnostics: Vec<String>,
    correctness_violations: Vec<String>,
    decision: AbSequentialDecision,
    stop_reason: AbStopReason,
    passed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AbWorkloadReport {
    workload: String,
    workload_class: AbWorkloadClass,
    workload_shape: AbWorkloadShape,
    fixture_sha256: String,
    workload_schema_sha256: String,
    clusters: Vec<AbPairedCluster>,
    sequential_looks: Vec<AbSequentialLook>,
    latency_gates: Vec<AbLatencyGate>,
    latency_diagnostics: Vec<String>,
    latency_gate_mode: AbLatencyGateMode,
    correctness_violations: Vec<String>,
    status: AbRunStatus,
    stop_reason: AbStopReason,
    cap_expired: bool,
    stopped_at_pairs_per_cluster: usize,
    passed: bool,
    report_payload_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AbReport {
    schema_version: u16,
    workload: String,
    provenance: AbProvenance,
    requested_workloads: Vec<String>,
    selected_workloads: Vec<String>,
    unstarted_workloads: Vec<String>,
    workloads: Vec<AbWorkloadReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replay_candidate_contention_self_test: Option<AbReplayCandidateSelfTest>,
    status: AbRunStatus,
    cap_expired: bool,
    passed: bool,
    report_payload_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AbReplayCandidateSelfTest {
    executed: bool,
    passed: bool,
    expected_direct_calls: u32,
    expected_nested_calls: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    failure_codes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sample: Option<Sample>,
}

struct AbWorkloadVerdict {
    latency_gates: Vec<AbLatencyGate>,
    latency_diagnostics: Vec<String>,
    correctness_violations: Vec<String>,
    decision: AbSequentialDecision,
    stop_reason: AbStopReason,
    passed: bool,
}

struct AbWorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Receiver<std::result::Result<String, String>>,
}

impl Drop for AbWorkerProcess {
    fn drop(&mut self) {
        if self.child.try_wait().is_ok_and(|status| status.is_none()) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[derive(Clone, Debug)]
struct AbBuild {
    worktree: PathBuf,
    cli: PathBuf,
    host: PathBuf,
    worker: PathBuf,
}
