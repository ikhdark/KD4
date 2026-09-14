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

const REPORT_VERSION: u32 = 1;
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
    source_result_sha256: String,
    prepared_id: String,
    prepared: Prepared,
    result: RunResult,
    completion: Completion,
    comparisons: Vec<Comparison>,
    feature_coverage: Vec<FeatureCoverage>,
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
    let report = Report {
        schema_version: REPORT_VERSION,
        source_result_sha256: source_result_sha256.clone(),
        prepared_id: prepared.id.clone(),
        prepared: prepared.clone(),
        result: result.clone(),
        completion: completion(&result.attempts),
        comparisons: summarize(&observations(&result.attempts)),
        feature_coverage: feature_coverage(prepared, result),
        measurement_notes: vec![
            "Performance measurements have no pass/fail verdict. Failed tasks and invalid setup remain failures.".into(),
            "One real-model attempt per task and variant is an observed comparison, not repeat-to-repeat model variance. Different tasks are not repetitions.".into(),
            "Fork-off versus reference measures drift, including instrumentation and fixed fork changes; it does not establish parity.".into(),
            "Completed timing excludes failed, unverified, unrun, and warmup observations. Their original evidence and exclusion reasons remain visible.".into(),
            "Comparison tables reuse original sample IDs. Pairing requires the same workload, run cluster, and repetition; timestamps remain in result.attempts.".into(),
            "Python owns diagnostic and token definitions. Missing native telemetry is unavailable, never zero. Scripted token analysis is disabled.".into(),
            "Execution durations are ceilings. Preparation, builds, resets, independent verification, cleanup and analysis are recorded outside execution budgets.".into(),
        ],
    };
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
        result.schema_version == REPORT_VERSION,
        "unsupported result schema; prepare again for Repo Benchmark"
    );
    ensure!(
        prepared.schema_version == REPORT_VERSION,
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
                    && native.schema_version == REPORT_VERSION,
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
            // Placeholder for excluded observations only. It can never enter a
            // completed distribution, and the original elapsed field stays absent.
            elapsed_ms: attempt
                .native
                .as_ref()
                .map_or(0, |native| native.elapsed_ms),
        })
        .collect()
}

fn feature_coverage(prepared: &Prepared, result: &RunResult) -> Vec<FeatureCoverage> {
    prepared.features.iter().map(|feature| {
        let keys: Vec<_> = feature["config_keys"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let id = feature["id"].as_str().unwrap_or("unknown").to_string();
        let mut configured_by_variant = BTreeMap::new();
        let mut observed_effective_by_variant = BTreeMap::new();
        for variant in Variant::ALL {
            let configured: Option<Vec<bool>> = keys.iter().map(|key| {
                prepared.overrides.get(&variant)?.iter().find_map(|setting| {
                    let (name, value) = setting.split_once('=')?;
                    (name.trim() == *key).then(|| value.trim().parse().ok()).flatten()
                })
            }).collect();
            configured_by_variant.insert(variant, configured.filter(|values| !values.is_empty()).map(|values| values.iter().all(|value| *value)));
            let observed: Vec<_> = result.attempts.iter().filter(|attempt| attempt.scheduled.variant == variant).filter_map(|attempt| attempt.native.as_ref()).filter(|native| !native.effective_config.is_null()).collect();
            let values: Option<Vec<bool>> = observed.iter().map(|native| feature_value(&native.effective_config, &keys)).collect();
            // Mixed effective values mean unavailable as a single variant state.
            let value = values.filter(|values| !values.is_empty()).and_then(|values| values.iter().all(|value| *value == values[0]).then_some(values[0]));
            observed_effective_by_variant.insert(variant, value);
        }
        FeatureCoverage {
            id, declared: feature.clone(), control: feature["benchmark_control"].clone(),
            configured_by_variant, observed_effective_by_variant,
            exercised: None, exercise_evidence: vec![],
            note: "Declared/configured/effective settings do not prove exercise. No compatible per-feature exercise event is available; consult native events and Python phase/tool evidence without inferring coverage from enabled flags.".into(),
        }
    }).collect()
}

fn feature_value(config: &Value, keys: &[&str]) -> Option<bool> {
    if keys.is_empty() {
        return None;
    }
    let values: Option<Vec<bool>> = keys
        .iter()
        .map(|key| {
            let pointer = format!("/{}", key.replace('.', "/"));
            config.pointer(&pointer).and_then(Value::as_bool)
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
        "\nScripted execution: {} ms; real-model execution: {} ms. Preparation: {} ms. Per-attempt outside-execution durations are preserved in JSON.\n",
        report.result.scripted_execution_ms,
        report.result.real_model_execution_ms,
        report.prepared.preparation_ms
    );
    output.push_str("Differences are candidate minus baseline; ratios are candidate / baseline. The table describes all completed observations. Intervals below use only recorded matched pairs.\n\n| Segment / workload | Comparison | Candidate / baseline | Counts | Median difference ms | Median ratio | p95 difference ms | p95 ratio |\n|---|---|---|---|---:|---:|---:|---:|\n");
    for comparison in &report.comparisons {
        let observed = comparison.observed.as_ref();
        let _ = writeln!(
            output,
            "| {:?} / {} | {} | {} / {} | {} / {} | {} | {} | {} | {} |",
            comparison.segment,
            cell(&comparison.workload),
            comparison.kind,
            label(comparison.candidate, report.prepared.reference.upstream),
            label(comparison.baseline, report.prepared.reference.upstream),
            comparison.candidate_distribution.count,
            comparison.baseline_distribution.count,
            number(observed.map(|value| value.median_difference_ms)),
            number(observed.and_then(|value| value.median_ratio)),
            number(observed.map(|value| value.p95_difference_ms)),
            number(observed.and_then(|value| value.p95_ratio))
        );
    }
    output.push_str("\n| Workload / comparison | Paired observations | Clusters | 95% paired median difference interval ms | Median ratio interval | Uncertainty status |\n|---|---:|---:|---|---|---|\n");
    for comparison in &report.comparisons {
        let (clusters, difference, ratio, status) = if let Some(bootstrap) = &comparison.bootstrap {
            (
                bootstrap.cluster_count.to_string(),
                format!(
                    "[{:.3}, {:.3}]",
                    bootstrap.median_difference_ms.lower, bootstrap.median_difference_ms.upper
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
                "unavailable".into(),
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
            "| {} / {} | {} | {} | {} | {} | {} |",
            cell(&comparison.workload),
            comparison.kind,
            comparison.pairs.len(),
            clusters,
            difference,
            ratio,
            status
        );
    }
    output.push_str("\nFull distributions, p95 intervals, sample IDs, excluded observations and shared sample identities are preserved in report.json.\n\n## Feature coverage\n\n| Feature | Control | Configured fork_off / fork_on / reference | Observed exercised |\n|---|---|---|---|\n");
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
            "| {} | {} | {} | unavailable; no per-feature exercise evidence |",
            cell(&feature.id),
            cell(feature.control["kind"].as_str().unwrap_or("unavailable")),
            states.join(" / ")
        );
    }
    output.push_str("\nEnabled settings are not exercise evidence. Complete declarations, observed effective settings and fixed-difference explanations remain in JSON. Instrumentation and other fixed fork modifications remain in fork_off.\n\n## Diagnostics\n\nThese values come directly from the frozen Python analyzer. They are not recomputed in Rust. Each session retains its coverage and accounting basis; session rows are not summed, preventing duplicate cumulative usage.\n\n");
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
        "physicalRequests",
        "directToolCount",
        "nestedToolCount",
        "toolCountCoverage",
        "firstOutputMs",
        "firstToolMs",
        "lastProgress",
        "measurementNote",
    ] {
        output.insert(key.into(), source.get(key).cloned().unwrap_or(Value::Null));
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
        "longestEventGaps",
    ] {
        if let Some(rows) = source.get(key).and_then(Value::as_array) {
            output.insert(
                key.into(),
                json!(rows.iter().take(DIAGNOSTIC_TRACE_LIMIT).collect::<Vec<_>>()),
            );
            output.insert(
                format!("omitted{key}"),
                json!(rows.len().saturating_sub(DIAGNOSTIC_TRACE_LIMIT)),
            );
        } else {
            output.insert(key.into(), Value::Null);
        }
    }
    if live {
        output.insert(
            "tokens".into(),
            source.get("tokens").cloned().unwrap_or(Value::Null),
        );
    }
    Value::Object(output)
}

/// Copy reports into the fixed accepted directory; never rewrite original evidence.
pub fn import(result_path: &Path) -> Result<PathBuf> {
    let result: RunResult = read_json(result_path)?;
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
    fn diagnostics_display_preserves_python_accounting_and_discloses_truncation() {
        let report = json!({"runnerDiagnostics": {"logicalGenerations": 4, "tokens": {"inputTokens": 123, "promptCategories": null}, "failures": (0..12).map(|index| json!({"eventIndex": index, "kind":"tool_error"})).collect::<Vec<_>>(), "runtime": {"toolOnlyNs": 12, "modelToolOverlapNs": 7, "tokens": {"inputTokens": 123}}}});
        let live = diagnostic_values(&report, true);
        assert_eq!(live["logicalGenerations"], 4);
        assert_eq!(live["tokens"], report["runnerDiagnostics"]["tokens"]);
        assert_eq!(live["failures"].as_array().unwrap().len(), 8);
        assert_eq!(live["omittedfailures"], 4);
        assert_eq!(live["runtime"]["toolOnlyNs"], 12);
        assert_eq!(live["runtime"]["modelToolOverlapNs"], 7);
        let scripted = diagnostic_values(&report, false);
        assert!(scripted.get("tokens").is_none());
        assert!(scripted["runtime"].get("tokens").is_none());
        assert_eq!(scripted["physicalRequests"], Value::Null);
    }

    #[test]
    fn feature_settings_and_reference_labels_do_not_claim_exercise() {
        assert_eq!(
            feature_value(
                &json!({"features":{"kd4_runtime":true}}),
                &["features.kd4_runtime"]
            ),
            Some(true)
        );
        assert_eq!(
            feature_value(
                &json!({"features":{"kd4_runtime":false}}),
                &["features.kd4_runtime"]
            ),
            Some(false)
        );
        assert_eq!(feature_value(&json!({}), &["features.kd4_runtime"]), None);
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
