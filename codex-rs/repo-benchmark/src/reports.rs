//! Assemble preserved results and canonical Python diagnostics without model calls.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::prepare::Prepared;
use crate::prepare::provenance::{hash_file, read_json, write_json};
use crate::runner::{Attempt, RunResult};
use crate::schedule::{Segment, Variant};
use crate::statistics::{Comparison, Observation, summarize};
use crate::workloads::VerificationStatus;

const REPORT_VERSION: u32 = 2;
const RESULT_VERSION: u32 = 1;
const BEHAVIOR_SCHEMA_VERSION: u64 = 2;
const DIAGNOSTIC_TRACE_LIMIT: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Completion {
    scheduled: usize,
    executed: usize,
    completed: usize,
    failed: usize,
    unrun: usize,
    statuses: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeatureCoverage {
    id: String,
    declared: Value,
    control: Value,
    ablation_status: String,
    declared_verification: Value,
    configured_settings_by_variant: BTreeMap<Variant, BTreeMap<String, Option<bool>>>,
    configured_by_variant: BTreeMap<Variant, Option<bool>>,
    observed_effective_by_variant: BTreeMap<Variant, Option<bool>>,
    exercised: Option<bool>,
    exercise_evidence: Vec<Value>,
    note: String,
}

/// JSON and Markdown are derived from this one model, before display truncation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    schema_version: u32,
    behavior_schema_version: u64,
    source_result_sha256: String,
    prepared_id: String,
    prepared: Prepared,
    result: RunResult,
    completion: Completion,
    comparisons: Vec<Comparison>,
    feature_coverage: Vec<FeatureCoverage>,
    ablation_counts: BTreeMap<String, usize>,
    coupled_controls: BTreeMap<String, Vec<String>>,
    measurement_notes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReportHashes {
    schema_version: u32,
    run_id: String,
    prepared_id: String,
    source_result_sha256: String,
    prepared_manifest_sha256: String,
    report_json_sha256: String,
    report_markdown_sha256: String,
}

pub fn write(prepared: &Prepared, result: &RunResult) -> Result<()> {
    validate_result(prepared, result)?;
    let result_path = result.directory.join("result.json");
    let persisted: Value = read_json(&result_path)?;
    ensure!(
        persisted == serde_json::to_value(result)?,
        "persist raw result before generating reports"
    );
    let source_result_sha256 = hash_file(&result_path)?;
    let feature_coverage = feature_coverage(prepared, result);
    let mut ablation_counts = BTreeMap::from([
        ("runtime_changed".to_string(), 0),
        ("runtime_unchanged".to_string(), 0),
        ("not_ablated".to_string(), 0),
        ("configuration_unavailable".to_string(), 0),
    ]);
    let mut coupled_controls: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for feature in &feature_coverage {
        *ablation_counts
            .entry(feature.ablation_status.clone())
            .or_default() += 1;
        if feature.ablation_status == "runtime_changed" {
            for (key, value) in &feature.configured_settings_by_variant[&Variant::ForkOn] {
                if value != &feature.configured_settings_by_variant[&Variant::ForkOff][key] {
                    coupled_controls
                        .entry(key.clone())
                        .or_default()
                        .push(feature.id.clone());
                }
            }
        }
    }
    coupled_controls.retain(|_, features| features.len() > 1);
    let mut report = Report {
        schema_version: REPORT_VERSION,
        behavior_schema_version: BEHAVIOR_SCHEMA_VERSION,
        source_result_sha256: source_result_sha256.clone(),
        prepared_id: prepared.id.clone(),
        prepared: prepared.clone(),
        result: result.clone(),
        completion: completion(&result.attempts),
        comparisons: summarize(&observations(&result.attempts)),
        feature_coverage,
        ablation_counts,
        coupled_controls,
        measurement_notes: vec![
            "Behavior schema 2 compares exploratory effort in named units. Discovery metrics count recognized tool-call events, including a batch as one event. Complete session vectors are summed once across captured rollouts; any unavailable session measurement excludes that attempt for that metric. Unanchored turns and saturated counters are unavailable. Validation duration sums command wall time, not an elapsed union. Governor counts record interventions, not eligibility or mistakes. Planning counts do not establish plan quality, and wait counts do not establish user corrections; compare identical permission policies. Fewer actions do not prove better work or feature exercise.".into(),
            "Behavior comparisons have no significance test, acceptance gate or minimum detectable effect claim. Raw per-task distributions and paired differences are descriptive; causal attribution and model variance require independently repeated experiments.".into(),
            "Performance measurements have no pass/fail verdict. Failed tasks and invalid setup remain failures.".into(),
            "One real-model attempt per task and variant is an observed comparison, not repeat-to-repeat model variance. Different tasks are not repetitions.".into(),
            "Live latency is time to a candidate that passes external verification; model-performed test execution is not established by verifier success.".into(),
            "Fork-off versus reference measures drift, including instrumentation and fixed fork changes; it does not establish parity.".into(),
            "Completed timing excludes failed, unverified, unrun, and warmup observations. Their original evidence and exclusion reasons remain visible.".into(),
            "Comparison tables reuse original sample IDs. Pairing requires the same workload, run cluster, and repetition; timestamps remain in result.attempts.".into(),
            "Python owns diagnostic and token definitions. Missing or partial metric evidence is excluded per metric, never replaced by zero. Scripted request count and serialized request bytes are volume proxies; scripted token analysis remains disabled.".into(),
            "Intervals require at least five independent paired clusters. Default three-cluster scripted runs remain descriptive. Quantiles use linear interpolation at (n-1)*q; p95 with few samples is a descriptive tail estimate, not a stable population percentile.".into(),
            "Discovery counts are recognized operations, not distinct files or instruction-compliance verdicts. First-action turn medians require one fully covered rollout session. Complete request-token usage is read once per attempt; cumulative session snapshots are never summed.".into(),
            "Execution durations are ceilings. Preparation, builds, resets, independent verification, cleanup and analysis are recorded outside execution budgets.".into(),
        ],
    };
    for segment in [Segment::Scripted, Segment::RealModel] {
        let attempts: Vec<_> = result
            .attempts
            .iter()
            .filter(|attempt| attempt.scheduled.segment == segment && !attempt.scheduled.warmup)
            .collect();
        let scheduled = attempts.len();
        let completed_count = attempts.iter().filter(|attempt| completed(attempt)).count();
        let unrun = attempts
            .iter()
            .filter(|attempt| attempt.status == "not_started")
            .count();
        if scheduled > 0 && completed_count < scheduled {
            report.measurement_notes.push(format!(
                "{segment:?}: {} of {} measured attempts completed; {} failed and {} were unrun ({:.1}% incomplete). Available comparisons describe surviving measurements only; consult each metric's excluded-pair rate.",
                completed_count, scheduled, scheduled - completed_count - unrun, unrun,
                100.0 * (scheduled - completed_count) as f64 / scheduled as f64));
        }
    }
    let json_path = result.directory.join("report.json");
    let markdown_path = result.directory.join("report.md");
    write_json(&json_path, &report)?;
    fs::write(&markdown_path, render(&report))?;
    write_json(
        &result.directory.join("reports-manifest.json"),
        &ReportHashes {
            schema_version: REPORT_VERSION,
            run_id: result.id.clone(),
            prepared_id: prepared.id.clone(),
            source_result_sha256,
            prepared_manifest_sha256: result.prepared_manifest_sha256.clone(),
            report_json_sha256: hash_file(&json_path)?,
            report_markdown_sha256: hash_file(&markdown_path)?,
        },
    )?;
    Ok(())
}

fn validate_result(prepared: &Prepared, result: &RunResult) -> Result<()> {
    ensure!(
        result.schema_version == RESULT_VERSION,
        "unsupported result schema; prepare again for Repo Benchmark"
    );
    ensure!(
        prepared.schema_version == crate::prepare::MANIFEST_VERSION,
        "unsupported prepared schema"
    );
    ensure!(
        result.mode == prepared.mode,
        "result mode differs from preparation"
    );
    ensure!(
        hash_file(&result.prepared_manifest)? == result.prepared_manifest_sha256,
        "prepared manifest changed since execution"
    );
    let saved: Value = read_json(&result.prepared_manifest)?;
    ensure!(
        saved == serde_json::to_value(prepared)?,
        "provided preparation differs from the recorded manifest"
    );
    let mut ids = BTreeSet::new();
    for attempt in &result.attempts {
        ensure!(
            ids.insert(&attempt.scheduled.id),
            "duplicate result attempt {}",
            attempt.scheduled.id
        );
        ensure!(
            prepared.schedule.contains(&attempt.scheduled),
            "attempt {} differs from the prepared schedule",
            attempt.scheduled.id
        );
        if let Some(native) = &attempt.native {
            ensure!(
                native.attempt_id == attempt.scheduled.id
                    && native.schema_version == RESULT_VERSION,
                "native evidence identity mismatch for {}",
                attempt.scheduled.id
            );
        }
    }
    ensure!(
        result.original_run.is_some() || result.attempts.len() == prepared.schedule.len(),
        "result omitted scheduled attempts without a rerun origin"
    );
    Ok(())
}

fn completed(attempt: &Attempt) -> bool {
    attempt.status == "completed"
        && attempt
            .native
            .as_ref()
            .is_some_and(|native| native.status == "completed")
        && (attempt.scheduled.segment == Segment::Scripted
            || attempt
                .verifier
                .as_ref()
                .is_some_and(|verifier| verifier.status == VerificationStatus::Passed))
}

fn completion(attempts: &[Attempt]) -> Completion {
    let mut statuses = BTreeMap::new();
    for attempt in attempts {
        *statuses.entry(attempt.status.clone()).or_default() += 1;
    }
    let unrun = attempts
        .iter()
        .filter(|attempt| attempt.status == "not_started")
        .count();
    let completed = attempts.iter().filter(|attempt| completed(attempt)).count();
    Completion {
        scheduled: attempts.len(),
        executed: attempts.len() - unrun,
        completed,
        failed: attempts.len() - unrun - completed,
        unrun,
        statuses,
    }
}

fn observations(attempts: &[Attempt]) -> Vec<Observation> {
    attempts
        .iter()
        .map(|attempt| Observation {
            id: attempt.scheduled.id.clone(),
            segment: attempt.scheduled.segment,
            workload: attempt.scheduled.workload.clone(),
            variant: attempt.scheduled.variant,
            cluster: attempt.scheduled.cluster,
            repetition: attempt.scheduled.repetition,
            warmup: attempt.scheduled.warmup,
            completed: completed(attempt),
            metrics: attempt_metrics(attempt),
        })
        .collect()
}

fn attempt_metrics(attempt: &Attempt) -> BTreeMap<String, f64> {
    let mut metrics = BTreeMap::new();
    if let Some(native) = &attempt.native {
        metrics.insert("elapsed_ms".into(), native.elapsed_ms as f64);
        metrics.insert("tool_executions".into(), native.tool_executions as f64);
        metrics.insert("completed_turns".into(), native.completed_turns as f64);
    }
    for (phase, elapsed) in &attempt.outside_execution_ms {
        metrics.insert(format!("outside_{phase}_ms"), *elapsed as f64);
    }
    let Some(diagnostics) = &attempt.diagnostics else {
        return metrics;
    };
    if diagnostics.status != "available" {
        return metrics;
    }
    // Runner diagnostics describe the entire attempt and are supplied once.
    // Never sum cumulative usage or runner events across rollout sessions.
    if let Some(report) = diagnostics.reports.first() {
        let runner = &report["runnerDiagnostics"];
        let dispatch = &runner["toolDispatch"];
        if dispatch["schemaVersion"] == 1 && dispatch["complete"] == true {
            for (name, key) in [
                ("tool_retries", "retryCount"),
                ("tool_reentries", "reentryCount"),
            ] {
                if let Some(value) = dispatch[key].as_u64() {
                    metrics.insert(name.into(), value as f64);
                }
            }
        }
        let activity = &runner["toolActivity"];
        if activity["schemaVersion"] == 1 && activity["available"] == true {
            for (name, key) in [
                ("observed_tool_items", "observedCount"),
                ("completed_tool_items", "completedCount"),
            ] {
                if let Some(value) = activity[key].as_f64() {
                    metrics.insert(name.into(), value);
                }
            }
            if let Some(kinds) = activity["byKind"].as_object() {
                for (kind, count) in kinds {
                    if let Some(value) = count.as_f64() {
                        metrics.insert(format!("observed_tool_items_{kind}"), value);
                    }
                }
            }
        }
        for (name, pointer) in [
            ("scripted_request_count", "/capturedRequests/requestCount"),
            (
                "scripted_serialized_request_bytes",
                "/capturedRequests/serializedRequestBytes",
            ),
            ("first_output_ms", "/firstOutputMs"),
            ("first_tool_ms", "/firstToolMs"),
        ] {
            if let Some(value) = runner.pointer(pointer).and_then(Value::as_f64) {
                metrics.insert(name.into(), value);
            }
        }
        let coverage = &runner["coverage"];
        let terminals = coverage["terminalTurns"].as_u64();
        if terminals.is_some_and(|count| count > 0)
            && terminals == coverage["validCompleteTimingProfiles"].as_u64()
        {
            for (name, key) in [
                ("physical_requests", "physicalRequests"),
                ("logical_generations", "logicalGenerations"),
            ] {
                if let Some(value) = runner[key].as_f64() {
                    metrics.insert(name.into(), value);
                }
            }
        }
        let tokens = &runner["tokens"];
        if tokens["complete"] == true && attempt.scheduled.segment == Segment::RealModel {
            if let Some(value) = runner["cacheHitRate"].as_f64() {
                metrics.insert("cache_hit_rate".into(), value);
            }
            for key in [
                "inputTokens",
                "cachedInputTokens",
                "outputTokens",
                "reasoningTokens",
                "totalTokens",
            ] {
                if let Some(value) = tokens[key].as_f64() {
                    metrics.insert(format!("tokens_{key}"), value);
                }
            }
        }
    }
    let reports = &diagnostics.reports;
    let sources: BTreeSet<_> = reports
        .iter()
        .filter_map(|report| report["source"].as_str())
        .collect();
    if !reports.is_empty()
        && sources.len() == reports.len()
        && reports.iter().all(|report| {
            report["behaviorMetrics"]["behaviorSchemaVersion"].as_u64()
                == Some(BEHAVIOR_SCHEMA_VERSION)
        })
    {
        for (name, key) in [
            ("discovery_events", "discoveryEvents"),
            ("discovery_searches", "searchEvents"),
            ("discovery_reads", "readEvents"),
            ("discovery_broad_searches", "broadSearchEvents"),
            ("discovery_repeated_searches", "repeatedSearchEvents"),
            ("behavior_executed_validations", "executedValidationCount"),
            (
                "behavior_validation_duration_ns",
                "executedValidationDurationNs",
            ),
            (
                "behavior_suppressed_validation_outputs",
                "suppressedValidationOutputCount",
            ),
            ("behavior_model_retries", "modelRetryCount"),
            ("behavior_model_fallbacks", "modelFallbackCount"),
            (
                "behavior_no_progress_directives",
                "noProgressDirectiveCount",
            ),
            (
                "behavior_proven_loop_activations",
                "provenLoopActivationCount",
            ),
            ("behavior_planning_generations", "planningGenerationCount"),
            (
                "behavior_plan_revision_generations",
                "planRevisionGenerationCount",
            ),
            (
                "behavior_planning_fixed_point_iterations",
                "planningFixedPointIterationCount",
            ),
            ("behavior_approval_waits", "approvalWaitCount"),
            ("behavior_permission_waits", "permissionWaitCount"),
            ("behavior_user_input_waits", "userInputWaitCount"),
            ("behavior_mcp_elicitation_waits", "mcpElicitationWaitCount"),
            (
                "behavior_tool_output_canonical_tokens",
                "toolOutputCanonicalTokenCount",
            ),
            (
                "behavior_tool_output_model_tokens",
                "toolOutputModelTokenCount",
            ),
            (
                "behavior_tool_output_recovery_calls",
                "toolOutputRecoveryCallCount",
            ),
            (
                "behavior_tool_output_recovery_retruncations",
                "toolOutputRecoveryRetruncationCount",
            ),
            ("behavior_total_tokens", "totalTokens"),
        ] {
            if key == "totalTokens" && attempt.scheduled.segment == Segment::Scripted {
                continue;
            }
            let total = reports.iter().try_fold(0_u64, |sum, report| {
                sum.checked_add(report["behaviorMetrics"]["metrics"][key].as_u64()?)
            });
            if let Some(total) = total {
                metrics.insert(name.into(), total as f64);
            }
        }
    }
    // Missing/malformed rollouts cannot prove the absence of discovery work.
    if !reports.is_empty()
        && reports.iter().all(|report| {
            report["coverage"]["files"]
                .as_u64()
                .is_some_and(|count| count > 0)
                && report["coverage"]["parseErrorCount"] == 0
        })
    {
        // A median of session medians is not an attempt-level turn median.
        if reports.len() == 1 {
            let first = &reports[0]["firstUsefulActionAnalysis"];
            let turns = first["completedTurnCount"].as_u64();
            for (name, key) in [
                (
                    "turn_median_first_useful_action_ms",
                    "startToFirstUsefulActionMs",
                ),
                (
                    "turn_median_first_domain_action_ms",
                    "startToFirstDomainActionMs",
                ),
            ] {
                let summary = &first["canonical"][key];
                if turns.is_some_and(|count| count > 0) && turns == summary["count"].as_u64() {
                    if let Some(value) = summary["p50"].as_f64() {
                        metrics.insert(name.into(), value);
                    }
                }
            }
        }
    }
    metrics
}

fn feature_coverage(prepared: &Prepared, result: &RunResult) -> Vec<FeatureCoverage> {
    prepared.features.iter().map(|feature| {
        let keys: Vec<_> = feature["config_keys"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let id = feature["id"].as_str().unwrap_or("unknown").to_string();
        let mut configured_by_variant = BTreeMap::new();
        let mut configured_settings_by_variant = BTreeMap::new();
        let mut observed_effective_by_variant = BTreeMap::new();
        for variant in Variant::ALL {
            let settings: BTreeMap<String, Option<bool>> = keys.iter().map(|key| {
                let value = prepared.overrides.get(&variant).and_then(|overrides| overrides.iter().find_map(|setting| {
                    let (name, value) = setting.split_once('=')?;
                    (name.trim() == *key).then(|| value.trim().parse().ok()).flatten()
                }));
                ((*key).to_string(), value)
            }).collect();
            let configured: Option<Vec<bool>> = settings.values().copied().collect();
            configured_settings_by_variant.insert(variant, settings);
            configured_by_variant.insert(variant, configured.filter(|values| !values.is_empty()).map(|values| values.iter().all(|value| *value)));
            let observed: Vec<_> = result.attempts.iter().filter(|attempt| attempt.scheduled.variant == variant).filter_map(|attempt| attempt.native.as_ref()).filter(|native| !native.effective_config.is_null()).collect();
            let values: Option<Vec<bool>> = observed.iter().map(|native| feature_value(&native.effective_config, &keys)).collect();
            // Mixed effective values mean unavailable as a single variant state.
            let value = values.filter(|values| !values.is_empty()).and_then(|values| values.iter().all(|value| *value == values[0]).then_some(values[0]));
            observed_effective_by_variant.insert(variant, value);
        }
        let ablation_status = if feature["benchmark_control"]["kind"] != "runtime" {
            "not_ablated"
        } else if keys.is_empty() || [Variant::ForkOff, Variant::ForkOn].iter().any(|variant| configured_settings_by_variant[variant].values().any(Option::is_none)) {
            "configuration_unavailable"
        } else if configured_settings_by_variant[&Variant::ForkOff] == configured_settings_by_variant[&Variant::ForkOn] {
            "runtime_unchanged"
        } else {
            "runtime_changed"
        };
        FeatureCoverage {
            id, declared: feature.clone(), control: feature["benchmark_control"].clone(),
            ablation_status: ablation_status.into(),
            declared_verification: feature["runtime_verification"].clone(),
            configured_settings_by_variant,
            configured_by_variant, observed_effective_by_variant,
            exercised: None, exercise_evidence: vec![],
            note: "Ablation status describes configured changes, not exercise. Uncontrolled runtime changes remain in both fork builds; repository-only declarations need not execute in either arm. Declared verification is a test reference, not a gate result at this revision or benchmark exercise evidence.".into(),
        }
    }).collect()
}

fn feature_value(config: &Value, keys: &[&str]) -> Option<bool> {
    if keys.is_empty() {
        return None;
    }
    // NativeAttemptEvidence preserves the full config/read result, including
    // its layers metadata; feature settings live inside its config object.
    let config = config.get("config")?.as_object()?;
    let values: Option<Vec<bool>> = keys
        .iter()
        .map(|key| {
            let mut parts = key.split('.');
            let mut value = config.get(parts.next()?)?;
            for part in parts {
                value = value.get(part)?;
            }
            value.as_bool()
        })
        .collect();
    values.map(|values| values.iter().all(|value| *value))
}

fn label(variant: Variant, upstream: bool) -> &'static str {
    if variant == Variant::Reference && upstream {
        "reference (upstream)"
    } else {
        variant.name()
    }
}

fn cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn number(value: Option<f64>) -> String {
    value.map_or_else(|| "unavailable".into(), |value| format!("{value:.3}"))
}

fn render(report: &Report) -> String {
    let mut output = String::new();
    let counts = &report.completion;
    let _ = writeln!(
        output,
        "# Repo Benchmark\n\n{} completed; {} failed; {} unrun. Scheduled: {}; executed: {}.\n",
        counts.completed, counts.failed, counts.unrun, counts.scheduled, counts.executed
    );
    let _ = writeln!(
        output,
        "Mode: `{}`. Run: `{}`. Finished schedule processing: `{}`.\n",
        report.result.mode.flag(),
        report.result.id,
        report.result.finished
    );
    let _ = writeln!(
        output,
        "Fork: `{}`. {}: `{}`. Prepared identity: `{}`.\n",
        report.prepared.fork.revision,
        label(Variant::Reference, report.prepared.reference.upstream),
        report.prepared.reference.revision,
        report.prepared_id
    );
    output.push_str("## Configuration scope\n\n");
    if let Some(comparison) = &report.prepared.project_config_comparison {
        let _ = writeln!(
            output,
            "Explicit project settings captured from `{}` (SHA-256 `{}`). Each benchmark arm includes its feature overrides. This comparison excludes home configuration, defaults and other configuration layers; it does not establish equivalence to daily effective settings. Values are omitted.\n",
            cell(&comparison.path.display().to_string()),
            comparison.sha256
        );
        output.push_str("| Variant | Differing keys | Only in project | Only in benchmark | Different values |\n|---|---:|---|---|---|\n");
        for (variant, diff) in &comparison.by_variant {
            let _ = writeln!(
                output,
                "| {} | {} | {} | {} | {} |",
                label(*variant, report.prepared.reference.upstream),
                diff.project_only.len() + diff.benchmark_only.len() + diff.changed.len(),
                cell(&diff.project_only.join(", ")),
                cell(&diff.benchmark_only.join(", ")),
                cell(&diff.changed.join(", "))
            );
        }
        output.push('\n');
    } else {
        output.push_str("Project configuration comparison unavailable: no project config was captured during preparation. The fixed benchmark configuration does not establish equivalence to daily effective settings.\n\n");
    }
    let code_mode_hosts: Vec<_> =
        Variant::ALL
            .into_iter()
            .map(|variant| {
                (
                    variant,
                    report.prepared.builds.get(&variant).is_some_and(|build| {
                        build.executables.contains_key("codex-code-mode-host")
                    }),
                )
            })
            .collect();
    if code_mode_hosts.iter().any(|(_, present)| *present)
        && code_mode_hosts.iter().any(|(_, present)| !*present)
    {
        output.push_str("Code-mode host availability differs across variants. Nested-tool comparisons include this runtime capability difference.\n\n");
        for (variant, present) in code_mode_hosts {
            let _ = writeln!(
                output,
                "- {}: code-mode host {}",
                label(variant, report.prepared.reference.upstream),
                if present { "present" } else { "absent" }
            );
        }
        output.push('\n');
    }
    output.push_str("## Failures and unrun attempts\n\n");
    if counts.failed == 0 && counts.unrun == 0 {
        output.push_str("None.\n\n");
    } else {
        output.push_str(
            "| Attempt | Result status | Native status / elapsed ms | Recorded failure | Independent verification | Evidence |\n|---|---|---|---|---|---|\n",
        );
        for attempt in report
            .result
            .attempts
            .iter()
            .filter(|attempt| !completed(attempt))
        {
            let failure = failure_fields(attempt);
            let _ = writeln!(
                output,
                "| {} | {} | {} | {} | {} | {} |",
                cell(&attempt.scheduled.id),
                cell(&attempt.status),
                cell(&failure.0),
                cell(&failure.1),
                cell(&failure.2),
                cell(&attempt.evidence_directory.display().to_string())
            );
        }
        output.push('\n');
    }
    output.push_str("## Measurements and uncertainty\n\n");
    for note in &report.measurement_notes {
        let _ = writeln!(output, "- {note}");
    }
    let _ = writeln!(
        output,
        "\nScripted execution: {} ms; real-model execution: {} ms. Preparation: {} ms. Per-attempt outside-execution durations are compared separately.\n",
        report.result.scripted_execution_ms,
        report.result.real_model_execution_ms,
        report.prepared.preparation_ms
    );
    output.push_str("Differences are candidate minus baseline; ratios are candidate / baseline. Quantiles use linear interpolation at (n-1)*q in Rust and Python. The table describes completed observations with valid evidence for each metric. Intervals below use only recorded matched pairs.\n\n| Segment / workload | Metric (unit) | Comparison | Candidate / baseline | Counts | Median difference | Median ratio | Tail difference | Tail ratio | Tail basis (candidate / baseline) |\n|---|---|---|---|---|---:|---:|---:|---:|---|\n");
    for comparison in &report.comparisons {
        let observed = comparison.observed.as_ref();
        let tail = observed.filter(|_| {
            comparison.candidate_distribution.count >= 20
                && comparison.baseline_distribution.count >= 20
        });
        let _ = writeln!(
            output,
            "| {:?} / {} | {} ({}) | {} | {} / {} | {} / {} | {} | {} | {} | {} | {} / {} |",
            comparison.segment,
            cell(&comparison.workload),
            cell(&comparison.metric),
            comparison.unit,
            comparison.kind,
            label(comparison.candidate, report.prepared.reference.upstream),
            label(comparison.baseline, report.prepared.reference.upstream),
            comparison.candidate_distribution.count,
            comparison.baseline_distribution.count,
            number(observed.map(|value| value.median_difference)),
            number(observed.and_then(|value| value.median_ratio)),
            number(tail.map(|value| value.p95_difference)),
            number(tail.and_then(|value| value.p95_ratio)),
            tail_label(comparison.candidate_distribution.count),
            tail_label(comparison.baseline_distribution.count),
        );
    }
    output.push_str("\nExcluded pairs are non-warmup schedule slots without a valid matched pair, divided by slots represented in either arm.\n\n| Workload / metric / comparison | Paired / scheduled slots | Excluded pairs | Clusters | 95% paired median difference interval (metric units) | Median ratio interval | Uncertainty status |\n|---|---:|---:|---:|---|---|---|\n");
    for comparison in &report.comparisons {
        let (clusters, difference, ratio, status) = if let Some(bootstrap) = &comparison.bootstrap {
            (
                bootstrap.cluster_count.to_string(),
                format!(
                    "[{:.3}, {:.3}]",
                    bootstrap.median_difference.lower, bootstrap.median_difference.upper
                ),
                bootstrap.median_ratio.as_ref().map_or_else(
                    || "unavailable".into(),
                    |value| format!("[{:.3}, {:.3}]", value.lower, value.upper),
                ),
                format!(
                    "{} resamples; seed {}; {}",
                    bootstrap.replicates,
                    bootstrap.seed,
                    bootstrap
                        .ratio_unavailable_reason
                        .as_deref()
                        .unwrap_or("supported")
                ),
            )
        } else {
            (
                comparison.cluster_count.to_string(),
                "unavailable".into(),
                "unavailable".into(),
                comparison
                    .interval_unavailable_reason
                    .clone()
                    .unwrap_or_else(|| "unavailable".into()),
            )
        };
        let _ = writeln!(
            output,
            "| {} / {} / {} | {} / {} | {} | {} | {} | {} | {} |",
            cell(&comparison.workload),
            cell(&comparison.metric),
            comparison.kind,
            comparison.pairs.len(),
            comparison.scheduled_pair_slots,
            comparison.excluded_pair_rate.map_or_else(
                || "unavailable".into(),
                |rate| format!("{:.1}%", rate * 100.0)
            ),
            clusters,
            difference,
            ratio,
            status
        );
    }
    output.push_str("\nFull distributions, p95 intervals, sample IDs, excluded observations and shared sample identities are preserved in report.json.\n\n## Feature coverage\n\n");
    let _ = writeln!(
        output,
        "{} inventoried features: {} change runtime settings between fork arms; {} keep identical runtime settings; {} are not ablated; {} have unavailable configuration. These counts describe settings, not exercised features.\n",
        report.feature_coverage.len(),
        report.ablation_counts.get("runtime_changed").unwrap_or(&0),
        report
            .ablation_counts
            .get("runtime_unchanged")
            .unwrap_or(&0),
        report.ablation_counts.get("not_ablated").unwrap_or(&0),
        report
            .ablation_counts
            .get("configuration_unavailable")
            .unwrap_or(&0)
    );
    for (control, features) in &report.coupled_controls {
        let _ = writeln!(
            output,
            "Shared control `{}` changes {} together: {}. The comparison cannot attribute effects to individual features.\n",
            cell(control),
            features.len(),
            cell(&features.join(", "))
        );
    }
    output.push_str("| Feature | Control / ablation | Configured fork_off / fork_on / reference | Declared verification (not a run result) |\n|---|---|---|---|\n");
    for feature in &report.feature_coverage {
        let states: Vec<_> = Variant::ALL
            .iter()
            .map(|variant| {
                feature
                    .configured_by_variant
                    .get(variant)
                    .copied()
                    .flatten()
                    .map_or_else(|| "unavailable".into(), |enabled| enabled.to_string())
            })
            .collect();
        let _ = writeln!(
            output,
            "| {} | {} / {} | {} | {} |",
            cell(&feature.id),
            cell(feature.control["kind"].as_str().unwrap_or("unavailable")),
            feature.ablation_status,
            if feature.ablation_status == "not_ablated" {
                "not controlled in either fork arm".into()
            } else {
                states.join(" / ")
            },
            cell(&feature.declared_verification["path"].as_str().map_or_else(
                || "unavailable".into(),
                |path| {
                    format!(
                        "{}::{}",
                        path,
                        feature.declared_verification["symbol"]
                            .as_str()
                            .unwrap_or("unspecified")
                    )
                }
            ))
        );
    }
    output.push_str("\nEnabled settings and declared test references are not exercise evidence or proof that gates passed at this revision. Exercise remains unavailable. Complete declarations, per-key settings, observed effective settings and fixed-difference explanations remain in JSON. Instrumentation and other fixed runtime modifications remain in both fork arms; repository-only declarations need not execute in either.\n\n## Diagnostics\n\nThese values come directly from the frozen Python analyzer. They are not recomputed in Rust. Each session retains its coverage and accounting basis; session rows are not summed, preventing duplicate cumulative usage.\n\n");
    for attempt in &report.result.attempts {
        if attempt.scheduled.warmup {
            continue;
        }
        diagnostic_summary(&mut output, attempt);
    }
    output.push_str("## Evidence and reruns\n\nAll original inputs, revisions, configuration, native logs, events, timestamps, verification evidence and analysis logs are referenced by each attempt in report.json. Reruns preserve original evidence; real-model reruns may behave differently. Reports and analysis-only reruns require no model calls.\n\n");
    let _ = writeln!(
        output,
        "Analysis only: `just repo-benchmark rerun --result \"{}\" --analysis-only`\n",
        report.result.directory.join("result.json").display()
    );
    output.push_str("<details>\n<summary>Every scheduled attempt, evidence directory and rerun command</summary>\n\n| Attempt | Status | Started Unix ms | Evidence directory | Rerun |\n|---|---|---:|---|---|\n");
    for attempt in &report.result.attempts {
        let _ = writeln!(
            output,
            "| {} | {} | {} | {} | `{}` |",
            cell(&attempt.scheduled.id),
            cell(&attempt.status),
            attempt
                .started_unix_ms
                .map_or_else(|| "not started".into(), |value| value.to_string()),
            cell(&attempt.evidence_directory.display().to_string()),
            cell(&attempt.rerun_command)
        );
    }
    output.push_str("\n</details>\n\nDisplay limits apply after statistics are calculated. Warmup diagnostic summaries are omitted; full diagnostics remain in JSON. Detailed diagnostic arrays display at most eight entries per field and disclose omitted counts. Native traces remain in the original evidence files.\n");
    output
}

fn tail_label(count: usize) -> &'static str {
    match count {
        0 => "unavailable",
        1..=19 => "suppressed (n<20)",
        _ => "p95",
    }
}

fn diagnostic_summary(output: &mut String, attempt: &Attempt) {
    let _ = writeln!(
        output,
        "<details>\n<summary>{}: diagnostics {}</summary>\n",
        cell(&attempt.scheduled.id),
        attempt
            .diagnostics
            .as_ref()
            .map_or("unavailable", |diagnostics| diagnostics.status.as_str())
    );
    let failure = failure_fields(attempt);
    let _ = writeln!(
        output,
        "Result status: `{}`. Native execution: {}.\n\nRecorded reason: {}.\n\nIndependent verification: {}.\n",
        cell(&attempt.status),
        cell(&failure.0),
        cell(&failure.1),
        cell(&failure.2),
    );
    if let Some(diagnostics) = &attempt.diagnostics {
        if let Some(error) = &diagnostics.error {
            let _ = writeln!(output, "Analyzer error: {}\n", cell(error));
        }
        for (index, report) in diagnostics.reports.iter().enumerate() {
            let summary =
                diagnostic_values(report, attempt.scheduled.segment == Segment::RealModel);
            let _ = writeln!(
                output,
                "Session {index}:\n\n```json\n{}\n```\n",
                serde_json::to_string_pretty(&summary).unwrap_or_else(|_| "null".into())
            );
        }
        let _ = writeln!(
            output,
            "Analysis stdout: {:?}\n\nAnalysis stderr: {:?}\n",
            diagnostics.stdout_paths, diagnostics.stderr_paths
        );
    } else {
        output.push_str("No diagnostic result is available; the attempt and raw evidence remain valid records.\n\n");
    }
    output.push_str("</details>\n\n");
}

/// Verification has its own outcome; an incorrect final workspace must not hide
/// the timeout, crash or failed tool that preceded independent verification.
fn failure_fields(attempt: &Attempt) -> (String, String, String) {
    let native = attempt.native.as_ref().map_or_else(
        || "unavailable".into(),
        |native| format!("{} / {} ms", native.status, native.elapsed_ms),
    );
    let mut reasons = Vec::new();
    if let Some(reason) = &attempt.reason {
        reasons.push(reason.clone());
    }
    if let Some(failure) = attempt
        .native
        .as_ref()
        .and_then(|native| native.failure.as_ref())
    {
        let reason = format!("{}: {}", failure.kind, failure.message);
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
    }
    let reason = if reasons.is_empty() {
        "No confirmed cause recorded".into()
    } else {
        reasons.join("; ")
    };
    let verifier = attempt.verifier.as_ref().map_or_else(
        || "unavailable".into(),
        |verifier| {
            format!(
                "{:?}: {} ({} ms outside execution)",
                verifier.status, verifier.detail, verifier.elapsed_ms
            )
        },
    );
    (native, reason, verifier)
}

/// Select existing Python fields only. Do not calculate usage or timing here.
fn diagnostic_values(report: &Value, live: bool) -> Value {
    let source = &report["runnerDiagnostics"];
    let mut output = serde_json::Map::new();
    for key in [
        "coverage",
        "units",
        "logicalGenerations",
        "requestClassification",
        "physicalRequests",
        "capturedRequests",
        "directToolCount",
        "nestedToolCount",
        "toolCountCoverage",
        "toolDispatch",
        "requestRetention",
        "cacheHitRate",
        "firstOutputMs",
        "firstToolMs",
        "lastProgress",
        "measurementNote",
    ] {
        output.insert(key.into(), source.get(key).cloned().unwrap_or(Value::Null));
    }
    if let Some(activity) = source.get("toolActivity").and_then(Value::as_object) {
        let mut activity = activity.clone();
        if let Some(turns) = activity.get("turns").and_then(Value::as_array) {
            let omitted = turns.len().saturating_sub(DIAGNOSTIC_TRACE_LIMIT);
            let shown = json!(
                turns
                    .iter()
                    .take(DIAGNOSTIC_TRACE_LIMIT)
                    .collect::<Vec<_>>()
            );
            activity.insert("turns".into(), shown);
            activity.insert("omittedTurns".into(), json!(omitted));
        }
        output.insert("toolActivity".into(), Value::Object(activity));
    }
    // The runtime object contains canonical exclusive timings and overlapping
    // activity separately. Rendering it does not add overlapping durations.
    if let Some(runtime) = source.get("runtime").and_then(Value::as_object) {
        let mut runtime = runtime.clone();
        if !live {
            runtime.remove("tokens");
            runtime.remove("observationalNonprogressTokens");
        }
        output.insert("runtime".into(), Value::Object(runtime));
    }
    for key in [
        "failures",
        "symptoms",
        "pendingTools",
        "retryEvidence",
        "generations",
        "tools",
        "longestEventGaps",
    ] {
        if let Some(rows) = source.get(key).and_then(Value::as_array) {
            output.insert(
                key.into(),
                json!(rows.iter().take(DIAGNOSTIC_TRACE_LIMIT).collect::<Vec<_>>()),
            );
            output.insert(
                format!(
                    "omitted{}{rest}",
                    key[..1].to_ascii_uppercase(),
                    rest = &key[1..]
                ),
                json!(rows.len().saturating_sub(DIAGNOSTIC_TRACE_LIMIT)),
            );
        } else {
            output.insert(key.into(), Value::Null);
        }
    }
    if live {
        for key in [
            "tokens",
            "tokenCoverage",
            "nativeCumulativeTokens",
            "tokenReconciliation",
        ] {
            output.insert(key.into(), source.get(key).cloned().unwrap_or(Value::Null));
        }
    }
    for key in [
        "behaviorMetrics",
        "sourceDiscovery",
        "firstUsefulActionAnalysis",
    ] {
        let mut summary = report.get(key).cloned().unwrap_or(Value::Null);
        if let Some(object) = summary.as_object_mut() {
            for key in ["events", "candidateSignals", "sourceSnapshots"] {
                if let Some(rows) = object.get_mut(key).and_then(Value::as_array_mut) {
                    let omitted = rows.len().saturating_sub(DIAGNOSTIC_TRACE_LIMIT);
                    rows.truncate(DIAGNOSTIC_TRACE_LIMIT);
                    let omitted_key = format!("omitted{}{}", key[..1].to_uppercase(), &key[1..]);
                    let previous = object
                        .get(&omitted_key)
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    object.insert(omitted_key, json!(previous + omitted as u64));
                }
            }
        }
        output.insert(key.into(), summary);
    }
    Value::Object(output)
}

/// Copy reports into the fixed accepted directory; never rewrite original evidence.
pub fn import(result_path: &Path) -> Result<PathBuf> {
    let result: RunResult = read_json(result_path)?;
    result.verify_evidence()?;
    let prepared = Prepared::load(&result.prepared_manifest)?;
    validate_result(&prepared, &result)?;
    ensure!(
        fs::canonicalize(result_path)? == fs::canonicalize(result.directory.join("result.json"))?,
        "result path differs from its recorded artifact location"
    );
    let manifest_path = result.directory.join("reports-manifest.json");
    let hashes: ReportHashes = read_json(&manifest_path)?;
    ensure!(
        hashes.schema_version == REPORT_VERSION,
        "unsupported report manifest schema"
    );
    ensure!(
        hashes.run_id == result.id && hashes.prepared_id == prepared.id,
        "report manifest identity mismatch"
    );
    ensure!(
        hashes.prepared_manifest_sha256 == result.prepared_manifest_sha256,
        "report preparation hash mismatch"
    );
    verify_hash(result_path, &hashes.source_result_sha256)?;
    let json_path = result.directory.join("report.json");
    let markdown_path = result.directory.join("report.md");
    verify_hash(&json_path, &hashes.report_json_sha256)?;
    verify_hash(&markdown_path, &hashes.report_markdown_sha256)?;
    let report: Report = read_json(&json_path)?;
    ensure!(
        report.schema_version == REPORT_VERSION,
        "unsupported report schema"
    );
    ensure!(
        report.prepared_id == prepared.id
            && report.source_result_sha256 == hashes.source_result_sha256,
        "report source identity mismatch"
    );
    ensure!(
        serde_json::to_value(&report.result)? == serde_json::to_value(&result)?,
        "report result differs from original evidence"
    );
    ensure!(
        serde_json::to_value(&report.prepared)? == serde_json::to_value(&prepared)?,
        "report preparation differs from frozen manifest"
    );
    ensure!(safe_id(&result.id), "unsafe report run identity");
    fs::create_dir_all(&prepared.import_directory)?;
    let destination = prepared.import_directory.join(&result.id);
    // Atomic directory creation rejects a prior import; file creation also never
    // follows a pre-existing target or overwrites a report.
    fs::create_dir(&destination)
        .context("import destination already exists or cannot be created")?;
    for (source, name) in [
        (&json_path, "report.json"),
        (&markdown_path, "report.md"),
        (&manifest_path, "reports-manifest.json"),
    ] {
        copy_new(source, &destination.join(name))?;
    }
    Ok(destination)
}

fn verify_hash(path: &Path, expected: &str) -> Result<()> {
    ensure!(
        hash_file(path)? == expected,
        "report evidence changed: {}",
        path.display()
    );
    Ok(())
}

fn safe_id(id: &str) -> bool {
    let mut components = Path::new(id).components();
    matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && !id.is_empty()
}

fn copy_new(source: &Path, destination: &Path) -> Result<()> {
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    output.write_all(&fs::read(source)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::ScheduledAttempt;

    fn attempt(id: &str, status: &str, reason: Option<&str>) -> Attempt {
        Attempt {
            scheduled: ScheduledAttempt {
                id: id.into(),
                segment: Segment::RealModel,
                workload: "rust_bugfix".into(),
                variant: Variant::ForkOn,
                cluster: 0,
                repetition: 0,
                warmup: false,
            },
            status: status.into(),
            reason: reason.map(str::to_string),
            started_unix_ms: None,
            native: None,
            verifier: None,
            diagnostics: None,
            outside_execution_ms: BTreeMap::new(),
            final_workspace_sha256: None,
            evidence_directory: PathBuf::from(id),
            evidence_files: vec![],
            rerun_command: format!("just repo-benchmark rerun --attempt {id}"),
        }
    }

    #[test]
    fn failures_and_unrun_attempts_remain_in_counts_and_excluded_measurements() {
        let attempts = [
            attempt("auth", "setup_failed", Some("authentication failed")),
            attempt("unrun", "not_started", Some("segment exhausted")),
            attempt("claim", "completed", None),
        ];
        let counts = completion(&attempts);
        assert_eq!(
            (
                counts.scheduled,
                counts.executed,
                counts.completed,
                counts.failed,
                counts.unrun
            ),
            (3, 2, 0, 2, 1)
        );
        assert_eq!(counts.statuses["setup_failed"], 1);
        let comparisons = summarize(&observations(&attempts));
        assert_eq!(comparisons[2].candidate_distribution.count, 0);
        assert!(
            comparisons[2]
                .exclusions
                .iter()
                .any(|excluded| excluded.sample_id == "auth")
        );
        assert!(
            comparisons[2]
                .exclusions
                .iter()
                .any(|excluded| excluded.sample_id == "unrun")
        );
        assert!(comparisons[2].observed.is_none());
    }

    #[test]
    fn native_activity_metrics_are_read_once_and_exclude_failed_attempts() {
        let mut sample = attempt("activity", "completed", None);
        sample.native = Some(
            serde_json::from_value(json!({
                "schemaVersion": 1, "attemptId": "activity", "status": "completed",
                "elapsedMs": 100, "cleanupMs": 0, "threadId": "root", "completedTurns": 1,
                "toolExecutions": 3, "failure": null, "effectiveConfig": null,
                "events": [], "stdoutPaths": [], "stderrPaths": [], "rolloutPaths": [],
                "providerRequestsPath": null, "adaptations": [], "evidencePath": "native.json"
            }))
            .unwrap(),
        );
        sample.verifier = Some(crate::workloads::VerificationOutcome {
            status: VerificationStatus::Passed,
            detail: "expected output verified".into(),
            elapsed_ms: 1,
            stdout_path: PathBuf::new(),
            stderr_path: PathBuf::new(),
        });
        let diagnostic = json!({"runnerDiagnostics": {
            "toolDispatch": {"schemaVersion": 1, "complete": true, "retryCount": 3, "reentryCount": 4},
            "toolActivity": {"schemaVersion": 1, "available": true, "observedCount": 7,
                "completedCount": 6, "byKind": {"commandExecution": 4, "fileChange": 1}},
            "tokens": {"complete": true, "inputTokens": 200, "cachedInputTokens": 80},
            "cacheHitRate": 0.4
        }});
        sample.diagnostics = Some(crate::diagnostics::DiagnosticResult {
            status: "available".into(),
            reports: vec![diagnostic.clone(), diagnostic],
            stdout_paths: vec![],
            stderr_paths: vec![],
            failed_sessions: vec![],
            error: None,
        });
        let metrics = attempt_metrics(&sample);
        assert_eq!(metrics["observed_tool_items"], 7.0);
        assert_eq!(metrics["completed_tool_items"], 6.0);
        assert_eq!(metrics["observed_tool_items_commandExecution"], 4.0);
        assert_eq!(metrics["observed_tool_items_fileChange"], 1.0);
        assert_eq!(metrics["cache_hit_rate"], 0.4);
        assert_eq!(metrics["tool_retries"], 3.0);
        assert_eq!(metrics["tool_reentries"], 4.0);
        let comparisons = summarize(&observations(&[sample.clone()]));
        let comparison = comparisons
            .iter()
            .find(|row| row.metric == "cache_hit_rate" && row.kind == "feature_effect")
            .unwrap();
        assert_eq!(comparison.unit, "fraction");
        assert_eq!(comparison.candidate_distribution.samples[0].value, 0.4);
        for status in ["failed", "timeout"] {
            let mut failed = sample.clone();
            failed.status = status.into();
            let comparisons = summarize(&observations(&[failed]));
            let comparison = comparisons
                .iter()
                .find(|row| row.metric == "observed_tool_items" && row.kind == "feature_effect")
                .unwrap();
            assert_eq!(comparison.candidate_distribution.count, 0);
            assert!(comparison.observed.is_none());
            assert_eq!(comparison.exclusions[0].reason, "not_completed");
        }
        sample.verifier.as_mut().unwrap().status = VerificationStatus::Incorrect;
        assert!(!observations(&[sample.clone()])[0].completed);
        sample.scheduled.segment = Segment::Scripted;
        assert!(!attempt_metrics(&sample).contains_key("cache_hit_rate"));
        let diagnostic = sample.diagnostics.as_mut().unwrap();
        diagnostic.reports[0]["runnerDiagnostics"]["toolActivity"]["available"] = json!(false);
        assert!(!attempt_metrics(&sample).contains_key("observed_tool_items"));
        sample.diagnostics.as_mut().unwrap().reports[0]["runnerDiagnostics"]["toolDispatch"]["complete"] =
            json!(false);
        assert!(!attempt_metrics(&sample).contains_key("tool_retries"));
        assert!(!attempt_metrics(&sample).contains_key("tool_reentries"));
    }

    #[test]
    fn behavior_metrics_require_supported_distinct_complete_sessions_per_metric() {
        let mut sample = attempt("behavior", "completed", None);
        let session = |source, searches, tokens| {
            json!({
                "source": source,
                "behaviorMetrics": {"behaviorSchemaVersion": 2, "metrics": {
                    "searchEvents": searches, "modelRetryCount": 0, "totalTokens": tokens,
                    "toolOutputCanonicalTokenCount": searches * 1000,
                    "toolOutputModelTokenCount": searches * 100,
                    "toolOutputRecoveryCallCount": searches,
                    "toolOutputRecoveryRetruncationCount": 0
                }}
            })
        };
        sample.diagnostics = Some(crate::diagnostics::DiagnosticResult {
            status: "available".into(),
            reports: vec![session("root.jsonl", 3, 120), session("child.jsonl", 2, 80)],
            stdout_paths: vec![],
            stderr_paths: vec![],
            failed_sessions: vec![],
            error: None,
        });
        let metrics = attempt_metrics(&sample);
        assert_eq!(metrics["discovery_searches"], 5.0);
        assert_eq!(metrics["behavior_model_retries"], 0.0);
        assert_eq!(metrics["behavior_total_tokens"], 200.0);
        for (name, total) in [
            ("behavior_tool_output_canonical_tokens", 5000.0),
            ("behavior_tool_output_model_tokens", 500.0),
            ("behavior_tool_output_recovery_calls", 5.0),
            ("behavior_tool_output_recovery_retruncations", 0.0),
        ] {
            assert_eq!(metrics[name], total);
        }
        assert!(!metrics.contains_key("discovery_reads"));
        let comparisons = summarize(&observations(&[sample.clone()]));
        for (name, unit) in [
            ("behavior_tool_output_canonical_tokens", "tokens"),
            ("behavior_tool_output_model_tokens", "tokens"),
            ("behavior_tool_output_recovery_calls", "count"),
            ("behavior_tool_output_recovery_retruncations", "count"),
        ] {
            let comparison = comparisons.iter().find(|row| row.metric == name).unwrap();
            assert_eq!(comparison.unit, unit);
            for value in [Value::Null, json!(-1)] {
                let mut missing = sample.clone();
                let key = match name {
                    "behavior_tool_output_canonical_tokens" => "toolOutputCanonicalTokenCount",
                    "behavior_tool_output_model_tokens" => "toolOutputModelTokenCount",
                    "behavior_tool_output_recovery_calls" => "toolOutputRecoveryCallCount",
                    _ => "toolOutputRecoveryRetruncationCount",
                };
                missing.diagnostics.as_mut().unwrap().reports[1]["behaviorMetrics"]["metrics"]
                    [key] = value;
                let metrics = attempt_metrics(&missing);
                assert!(!metrics.contains_key(name));
                assert_eq!(metrics["behavior_total_tokens"], 200.0);
            }
        }
        assert_eq!(
            comparisons
                .iter()
                .find(|row| row.metric == "behavior_total_tokens")
                .unwrap()
                .unit,
            "tokens"
        );
        let mut scripted = sample.clone();
        scripted.scheduled.segment = Segment::Scripted;
        assert!(!attempt_metrics(&scripted).contains_key("behavior_total_tokens"));
        assert_eq!(
            attempt_metrics(&scripted)["behavior_tool_output_model_tokens"],
            500.0
        );
        for (key, value) in [("searchEvents", Value::Null), ("searchEvents", json!(-1))] {
            let mut missing = sample.clone();
            missing.diagnostics.as_mut().unwrap().reports[1]["behaviorMetrics"]["metrics"][key] =
                value;
            let metrics = attempt_metrics(&missing);
            assert!(!metrics.contains_key("discovery_searches"));
            assert_eq!(metrics["behavior_model_retries"], 0.0);
        }
        for invalid in ["partial", "version", "duplicate", "missing_source"] {
            let mut invalid_sample = sample.clone();
            let diagnostics = invalid_sample.diagnostics.as_mut().unwrap();
            match invalid {
                "partial" => diagnostics.status = "partial".into(),
                "version" => {
                    diagnostics.reports[1]["behaviorMetrics"]["behaviorSchemaVersion"] = json!(999)
                }
                "duplicate" => diagnostics.reports[1]["source"] = json!("root.jsonl"),
                _ => diagnostics.reports[1]["source"] = Value::Null,
            }
            assert!(attempt_metrics(&invalid_sample).is_empty(), "{invalid}");
        }
    }

    #[test]
    fn diagnostics_display_preserves_python_accounting_and_discloses_truncation() {
        let report = json!({"runnerDiagnostics": {"logicalGenerations": 4, "tokens": {"inputTokens": 123, "promptCategories": null}, "failures": (0..12).map(|index| json!({"eventIndex": index, "kind":"tool_error"})).collect::<Vec<_>>(), "requestClassification": {"requestRecords": 12, "primaryCounts": {"repair": 12}}, "generations": (0..12).map(|index| json!({"eventIndex": index})).collect::<Vec<_>>(), "runtime": {"toolOnlyNs": 12, "modelToolOverlapNs": 7, "tokens": {"inputTokens": 123}}}});
        let live = diagnostic_values(&report, true);
        assert_eq!(live["logicalGenerations"], 4);
        assert_eq!(live["tokens"], report["runnerDiagnostics"]["tokens"]);
        assert_eq!(live["failures"].as_array().unwrap().len(), 8);
        assert_eq!(live["omittedFailures"], 4);
        assert!(live.get("omittedfailures").is_none());
        assert_eq!(live["requestClassification"]["requestRecords"], 12);
        assert_eq!(live["requestClassification"]["primaryCounts"]["repair"], 12);
        assert_eq!(live["generations"].as_array().unwrap().len(), 8);
        assert_eq!(live["omittedGenerations"], 4);
        assert_eq!(live["runtime"]["toolOnlyNs"], 12);
        assert_eq!(live["runtime"]["modelToolOverlapNs"], 7);
        let scripted = diagnostic_values(&report, false);
        assert!(scripted.get("tokens").is_none());
        assert!(scripted["runtime"].get("tokens").is_none());
        assert_eq!(scripted["physicalRequests"], Value::Null);
        let report = json!({"runnerDiagnostics": {"toolActivity": {"observedCount": 12,
            "turns": (0..12).map(|turn| json!({"turnId": turn})).collect::<Vec<_>>()}}});
        let shown = diagnostic_values(&report, true);
        assert_eq!(shown["toolActivity"]["observedCount"], 12);
        assert_eq!(shown["toolActivity"]["turns"].as_array().unwrap().len(), 8);
        assert_eq!(shown["toolActivity"]["omittedTurns"], 4);
    }

    #[test]
    fn feature_settings_and_reference_labels_do_not_claim_exercise() {
        assert_eq!(
            feature_value(
                &json!({"config":{"features":{"kd4_runtime":true}},"layers":[]}),
                &["features.kd4_runtime"]
            ),
            Some(true)
        );
        assert_eq!(
            feature_value(
                &json!({"config":{"features":{"kd4_runtime":false}},"layers":[]}),
                &["features.kd4_runtime"]
            ),
            Some(false)
        );
        assert_eq!(feature_value(&json!({}), &["features.kd4_runtime"]), None);
        assert_eq!(
            feature_value(
                &json!({"features":{"kd4_runtime":true}}),
                &["features.kd4_runtime"]
            ),
            None
        );
        assert_eq!(label(Variant::Reference, true), "reference (upstream)");
        assert_eq!(label(Variant::Reference, false), "reference");
    }

    #[test]
    fn incorrect_verification_does_not_hide_native_timeout_or_diagnoses() {
        let mut attempt = attempt(
            "timed-out",
            "incorrect",
            Some("segment_budget_exhausted during attempt"),
        );
        attempt.native = Some(
            serde_json::from_value(json!({
                "schemaVersion": 1, "attemptId": "timed-out", "status": "timeout",
                "elapsedMs": 600000, "cleanupMs": 12, "threadId": "thread-1",
                "completedTurns": 0, "toolExecutions": 2,
                "failure": {"kind":"attempt_timeout", "message":"tool process did not complete"},
                "effectiveConfig": {}, "events": [], "stdoutPaths": [], "stderrPaths": [],
                "rolloutPaths": [], "providerRequestsPath": null, "adaptations": [],
                "evidencePath":"native-evidence.json"
            }))
            .unwrap(),
        );
        attempt.verifier = Some(crate::workloads::VerificationOutcome {
            status: VerificationStatus::Incorrect,
            elapsed_ms: 82,
            detail: "required fix is missing".into(),
            stdout_path: "verify.stdout".into(),
            stderr_path: "verify.stderr".into(),
        });
        attempt.diagnostics = Some(crate::diagnostics::DiagnosticResult {
            status: "complete".into(),
            reports: vec![json!({"runnerDiagnostics":{
                "logicalGenerations": 3,
                "pendingTools":[{"callId":"call-2", "command":"rg target"}],
                "retryEvidence":[{"eventIndex":7, "cause":"output_cutoff"}],
                "tokens":{"inputTokens":77, "promptCategories":null}
            }})],
            stdout_paths: vec![],
            stderr_paths: vec![],
            failed_sessions: vec![],
            error: None,
        });
        let mut output = String::new();
        diagnostic_summary(&mut output, &attempt);
        for expected in [
            "incorrect",
            "timeout / 600000 ms",
            "segment_budget_exhausted",
            "attempt_timeout: tool process did not complete",
            "Incorrect: required fix is missing",
            "82 ms outside execution",
            "rg target",
            "output_cutoff",
            "\"logicalGenerations\": 3",
            "\"inputTokens\": 77",
        ] {
            assert!(
                output.contains(expected),
                "missing independent evidence {expected}: {output}"
            );
        }
        assert!(!completed(&attempt));
        assert_eq!(completion(&[attempt]).failed, 1);
    }

    #[test]
    fn import_rejects_changed_bytes_and_preserves_existing_report() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.json");
        let destination = temp.path().join("destination.json");
        fs::write(&source, "original").unwrap();
        let hash = hash_file(&source).unwrap();
        verify_hash(&source, &hash).unwrap();
        copy_new(&source, &destination).unwrap();
        fs::write(&source, "tampered").unwrap();
        assert!(verify_hash(&source, &hash).is_err());
        assert!(copy_new(&source, &destination).is_err());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "original");
        assert!(safe_id("run-123"));
        assert!(!safe_id("../escape"));
        assert!(!safe_id("nested/run"));
    }
}
