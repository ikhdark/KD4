//! Measurements for the frozen schedule. No performance acceptance thresholds.
use std::collections::{BTreeMap, BTreeSet};

use rand::{Rng, SeedableRng, rngs::StdRng};
use serde::{Deserialize, Serialize};

use crate::schedule::{Segment, Variant};

pub const BOOTSTRAP_REPLICATES: usize = 10_000;
pub const BOOTSTRAP_SEED: u64 = 0x4b44_345f_4142_7631;
// An explicit reporting floor, not a claim that five clusters give high power.
pub const MIN_INTERVAL_CLUSTERS: usize = 5;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    pub id: String,
    pub segment: Segment,
    pub workload: String,
    pub variant: Variant,
    pub cluster: u32,
    pub repetition: u32,
    pub warmup: bool,
    pub completed: bool,
    pub metrics: BTreeMap<String, f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Distribution {
    pub count: usize,
    /// Sorted by metric value; IDs stay attached to their measurements.
    pub samples: Vec<Sample>,
    pub median: Option<f64>,
    pub p95: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub id: String,
    pub value: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contrast {
    /// Candidate minus baseline, in the named metric's units.
    pub median_difference: f64,
    pub p95_difference: f64,
    /// Candidate / baseline; unavailable when the denominator is zero.
    pub median_ratio: Option<f64>,
    pub p95_ratio: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interval {
    pub lower: f64,
    pub upper: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bootstrap {
    pub confidence: f64,
    pub replicates: usize,
    pub seed: u64,
    pub cluster_count: usize,
    pub pair_count: usize,
    pub median_difference: Interval,
    pub p95_difference: Interval,
    pub median_ratio: Option<Interval>,
    pub p95_ratio: Option<Interval>,
    pub ratio_unavailable_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pair {
    pub cluster: u32,
    pub repetition: u32,
    pub baseline_id: String,
    pub candidate_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Exclusion {
    pub sample_id: String,
    pub variant: Variant,
    /// "completed_distribution" or "paired_statistics".
    pub scope: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comparison {
    pub segment: Segment,
    pub workload: String,
    pub metric: String,
    pub unit: String,
    pub kind: String,
    pub baseline: Variant,
    pub candidate: Variant,
    pub baseline_distribution: Distribution,
    pub candidate_distribution: Distribution,
    /// Descriptive comparison of all completed observations, including unpaired.
    pub observed: Option<Contrast>,
    /// Uses only the pairs listed below. Intervals describe this population.
    pub paired_observed: Option<Contrast>,
    pub pairs: Vec<Pair>,
    /// Distinct non-warmup schedule slots represented by either selected arm.
    pub scheduled_pair_slots: usize,
    pub excluded_pair_rate: Option<f64>,
    pub exclusions: Vec<Exclusion>,
    pub missing_variants: Vec<Variant>,
    /// These are the same original observations in another comparison table.
    pub reused_sample_ids: Vec<String>,
    pub cluster_count: usize,
    pub bootstrap: Option<Bootstrap>,
    pub interval_unavailable_reason: Option<String>,
    pub quantile_method: String,
}

const COMPARISONS: [(&str, Variant, Variant); 3] = [
    ("drift", Variant::Reference, Variant::ForkOff),
    ("feature_effect", Variant::ForkOff, Variant::ForkOn),
    ("overall", Variant::Reference, Variant::ForkOn),
];

/// Workloads never get pooled: different tasks are not repetitions.
pub fn summarize(observations: &[Observation]) -> Vec<Comparison> {
    let workloads: BTreeSet<_> = observations
        .iter()
        .map(|sample| (sample.segment, sample.workload.clone()))
        .collect();
    let mut results = Vec::new();
    for (segment, workload) in workloads {
        let samples: Vec<_> = observations
            .iter()
            .filter(|sample| sample.segment == segment && sample.workload == workload)
            .collect();
        let metrics: BTreeSet<_> = samples
            .iter()
            .flat_map(|sample| sample.metrics.keys().map(String::as_str))
            .chain(["elapsed_ms"])
            .collect();
        for metric in metrics {
            for (kind, baseline, candidate) in COMPARISONS {
                results.push(compare(
                    segment, &workload, metric, kind, baseline, candidate, &samples,
                ));
            }
        }
    }
    results
}

#[allow(clippy::too_many_arguments)]
fn compare(
    segment: Segment,
    workload: &str,
    metric: &str,
    kind: &str,
    baseline: Variant,
    candidate: Variant,
    samples: &[&Observation],
) -> Comparison {
    let selected: Vec<_> = samples
        .iter()
        .copied()
        .filter(|sample| sample.variant == baseline || sample.variant == candidate)
        .collect();
    let mut exclusions = Vec::new();
    let mut eligible = Vec::new();
    let mut ids = BTreeMap::new();
    for sample in &selected {
        *ids.entry(&sample.id).or_insert(0_usize) += 1;
    }
    for sample in &selected {
        let reason = if sample.warmup {
            Some("warmup")
        } else if !sample.completed {
            Some("not_completed")
        } else if ids.get(&sample.id).copied().unwrap_or_default() > 1 {
            Some("duplicate_sample_identity")
        } else if !sample.metrics.contains_key(metric) {
            Some("metric_unavailable")
        } else if !sample.metrics[metric].is_finite() || sample.metrics[metric] < 0.0 {
            Some("invalid_metric_value")
        } else {
            None
        };
        if let Some(reason) = reason {
            exclusions.push(exclude(sample, "completed_distribution", reason));
        } else {
            eligible.push(*sample);
        }
    }
    let baseline_distribution = distribution(&eligible, baseline, metric);
    let candidate_distribution = distribution(&eligible, candidate, metric);
    let observed = distribution_contrast(&baseline_distribution, &candidate_distribution);
    let mut slots: BTreeMap<(u32, u32), Vec<&Observation>> = BTreeMap::new();
    for sample in &eligible {
        slots
            .entry((sample.cluster, sample.repetition))
            .or_default()
            .push(sample);
    }
    let mut pairs = Vec::new();
    let mut clusters: BTreeMap<u32, Vec<(f64, f64)>> = BTreeMap::new();
    for ((cluster, repetition), slot) in slots {
        let a: Vec<_> = slot
            .iter()
            .filter(|sample| sample.variant == baseline)
            .collect();
        let b: Vec<_> = slot
            .iter()
            .filter(|sample| sample.variant == candidate)
            .collect();
        if a.len() == 1 && b.len() == 1 {
            pairs.push(Pair {
                cluster,
                repetition,
                baseline_id: a[0].id.clone(),
                candidate_id: b[0].id.clone(),
            });
            clusters
                .entry(cluster)
                .or_default()
                .push((a[0].metrics[metric], b[0].metrics[metric]));
        } else {
            let reason = if a.len() > 1 || b.len() > 1 {
                "ambiguous_schedule_slot"
            } else {
                "missing_completed_counterpart"
            };
            for sample in slot {
                exclusions.push(exclude(sample, "paired_statistics", reason));
            }
        }
    }
    let clusters: Vec<_> = clusters.into_values().collect();
    let (mut a, mut b): (Vec<_>, Vec<_>) = clusters.iter().flatten().copied().unzip();
    a.sort_by(f64::total_cmp);
    b.sort_by(f64::total_cmp);
    let paired_observed = contrast(&a, &b);
    // Within-run observations cannot substitute for independent run clusters.
    let interval_unavailable_reason =
        if metric.starts_with("behavior_") || metric.starts_with("discovery_") {
            Some("exploratory_behavior_counts".to_string())
        } else if pairs.len() < 2 {
            Some("insufficient_paired_repetitions".to_string())
        } else if clusters.len() < MIN_INTERVAL_CLUSTERS {
            Some("insufficient_independent_run_clusters".to_string())
        } else {
            None
        };
    let bootstrap = if interval_unavailable_reason.is_none() {
        Some(bootstrap(&clusters, metric))
    } else {
        None
    };
    let missing_variants = [baseline, candidate]
        .into_iter()
        .filter(|variant| !selected.iter().any(|sample| sample.variant == *variant))
        .collect();
    let reused_sample_ids = selected
        .iter()
        .map(|sample| sample.id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let scheduled_pair_slots = selected
        .iter()
        .filter(|sample| !sample.warmup)
        .map(|sample| (sample.cluster, sample.repetition))
        .collect::<BTreeSet<_>>()
        .len();
    let excluded_pair_rate = (scheduled_pair_slots > 0)
        .then(|| (scheduled_pair_slots - pairs.len()) as f64 / scheduled_pair_slots as f64);
    Comparison {
        segment,
        workload: workload.to_string(),
        metric: metric.to_string(),
        unit: if metric == "cache_hit_rate" {
            "fraction"
        } else if metric.ends_with("_ms") {
            "ms"
        } else if metric.ends_with("_ns") {
            "ns"
        } else if metric.ends_with("_bytes") {
            "bytes"
        } else if metric.starts_with("tokens_") || metric.ends_with("_tokens") {
            "tokens"
        } else {
            "count"
        }
        .into(),
        kind: kind.to_string(),
        baseline,
        candidate,
        baseline_distribution,
        candidate_distribution,
        observed,
        paired_observed,
        pairs,
        scheduled_pair_slots,
        excluded_pair_rate,
        exclusions,
        missing_variants,
        reused_sample_ids,
        cluster_count: clusters.len(),
        bootstrap,
        interval_unavailable_reason,
        quantile_method: "linear interpolation at (n-1)*q".to_string(),
    }
}

fn exclude(sample: &Observation, scope: &str, reason: &str) -> Exclusion {
    Exclusion {
        sample_id: sample.id.clone(),
        variant: sample.variant,
        scope: scope.to_string(),
        reason: reason.to_string(),
    }
}

fn distribution(samples: &[&Observation], variant: Variant, metric: &str) -> Distribution {
    let mut samples: Vec<_> = samples
        .iter()
        .filter(|sample| sample.variant == variant)
        .map(|sample| Sample {
            id: sample.id.clone(),
            value: sample.metrics[metric],
        })
        .collect();
    samples.sort_by(|a, b| a.value.total_cmp(&b.value).then(a.id.cmp(&b.id)));
    let values: Vec<_> = samples.iter().map(|sample| sample.value).collect();
    Distribution {
        count: samples.len(),
        median: percentile(&values, 0.5),
        p95: percentile(&values, 0.95),
        samples,
    }
}

fn distribution_contrast(a: &Distribution, b: &Distribution) -> Option<Contrast> {
    Some(contrast_values(a.median?, a.p95?, b.median?, b.p95?))
}

fn contrast(a: &[f64], b: &[f64]) -> Option<Contrast> {
    Some(contrast_values(
        percentile(a, 0.5)?,
        percentile(a, 0.95)?,
        percentile(b, 0.5)?,
        percentile(b, 0.95)?,
    ))
}

fn contrast_values(a_median: f64, a_p95: f64, b_median: f64, b_p95: f64) -> Contrast {
    Contrast {
        median_difference: b_median - a_median,
        p95_difference: b_p95 - a_p95,
        median_ratio: (a_median > 0.0).then(|| b_median / a_median),
        p95_ratio: (a_p95 > 0.0).then(|| b_p95 / a_p95),
    }
}

/// Match the canonical Python analyzer, including even-sized medians.
fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let position = (sorted.len() - 1) as f64 * q;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let weight = position - lower as f64;
    Some(sorted[lower] * (1.0 - weight) + sorted[upper] * weight)
}

fn interval(mut values: Vec<f64>) -> Interval {
    values.sort_by(f64::total_cmp);
    Interval {
        lower: percentile(&values, 0.025).expect("bootstrap samples"),
        upper: percentile(&values, 0.975).expect("bootstrap samples"),
    }
}

fn bootstrap(clusters: &[Vec<(f64, f64)>], metric: &str) -> Bootstrap {
    let seed = metric.as_bytes().iter().fold(BOOTSTRAP_SEED, |seed, byte| {
        seed.rotate_left(5) ^ u64::from(*byte)
    });
    let mut rng = StdRng::seed_from_u64(seed);
    let mut median_differences = Vec::with_capacity(BOOTSTRAP_REPLICATES);
    let mut p95_differences = Vec::with_capacity(BOOTSTRAP_REPLICATES);
    let mut median_ratios = Vec::with_capacity(BOOTSTRAP_REPLICATES);
    let mut p95_ratios = Vec::with_capacity(BOOTSTRAP_REPLICATES);
    for _ in 0..BOOTSTRAP_REPLICATES {
        let mut a = Vec::new();
        let mut b = Vec::new();
        for _ in 0..clusters.len() {
            let cluster = &clusters[rng.random_range(0..clusters.len())];
            // Preserve each selected cluster's observed paired population size.
            // Both sides sample the same pair, never independent observations.
            for _ in 0..cluster.len() {
                let pair = cluster[rng.random_range(0..cluster.len())];
                a.push(pair.0);
                b.push(pair.1);
            }
        }
        a.sort_by(f64::total_cmp);
        b.sort_by(f64::total_cmp);
        if let Some(estimate) = contrast(&a, &b) {
            median_differences.push(estimate.median_difference);
            p95_differences.push(estimate.p95_difference);
            if let Some(ratio) = estimate.median_ratio {
                median_ratios.push(ratio);
            }
            if let Some(ratio) = estimate.p95_ratio {
                p95_ratios.push(ratio);
            }
        }
    }
    let complete_ratios =
        median_ratios.len() == BOOTSTRAP_REPLICATES && p95_ratios.len() == BOOTSTRAP_REPLICATES;
    Bootstrap {
        confidence: 0.95,
        replicates: BOOTSTRAP_REPLICATES,
        seed,
        cluster_count: clusters.len(),
        pair_count: clusters.iter().map(Vec::len).sum(),
        median_difference: interval(median_differences),
        p95_difference: interval(p95_differences),
        median_ratio: if median_ratios.len() == BOOTSTRAP_REPLICATES {
            Some(interval(median_ratios))
        } else {
            None
        },
        p95_ratio: if p95_ratios.len() == BOOTSTRAP_REPLICATES {
            Some(interval(p95_ratios))
        } else {
            None
        },
        ratio_unavailable_reason: (!complete_ratios)
            .then(|| "zero_baseline_in_bootstrap_population".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(variant: Variant, cluster: u32, repetition: u32, elapsed_ms: u64) -> Observation {
        Observation {
            id: format!("{}-{cluster}-{repetition}", variant.name()),
            segment: Segment::Scripted,
            workload: "tool_dispatch".to_string(),
            variant,
            cluster,
            repetition,
            warmup: false,
            completed: true,
            metrics: BTreeMap::from([("elapsed_ms".into(), elapsed_ms as f64)]),
        }
    }

    #[test]
    fn comparison_quantiles_match_python_linear_interpolation_and_report_pair_coverage() {
        let mut observations: Vec<_> = [10, 20, 30, 40]
            .into_iter()
            .enumerate()
            .flat_map(|(cluster, value)| {
                [
                    sample(Variant::Reference, cluster as u32, 0, value),
                    sample(Variant::ForkOn, cluster as u32, 0, value + 20),
                ]
            })
            .collect();
        let results = summarize(&observations);
        let comparison = &results[2];
        assert_eq!(
            comparison.quantile_method,
            "linear interpolation at (n-1)*q"
        );
        assert_eq!(comparison.baseline_distribution.median, Some(25.0));
        assert_eq!(comparison.baseline_distribution.p95, Some(38.5));
        assert_eq!(
            comparison.observed.as_ref().unwrap().median_difference,
            20.0
        );
        assert_eq!(comparison.scheduled_pair_slots, 4);
        assert_eq!(comparison.excluded_pair_rate, Some(0.0));
        observations[1].completed = false;
        let results = summarize(&observations);
        assert_eq!(results[2].pairs.len(), 3);
        assert_eq!(results[2].scheduled_pair_slots, 4);
        assert_eq!(results[2].excluded_pair_rate, Some(0.25));
        assert_eq!(results[2].baseline_distribution.count, 4);
        assert_eq!(results[2].candidate_distribution.count, 3);
    }

    #[test]
    fn three_comparisons_have_explicit_direction_and_shared_identity() {
        let results = summarize(&[
            sample(Variant::Reference, 0, 0, 100),
            sample(Variant::ForkOff, 0, 0, 80),
            sample(Variant::ForkOn, 0, 0, 40),
        ]);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].kind, "drift");
        assert_eq!(
            results[0].observed.as_ref().unwrap().median_difference,
            -20.0
        );
        assert_eq!(
            results[1].observed.as_ref().unwrap().median_ratio,
            Some(0.5)
        );
        assert_eq!(results[2].observed.as_ref().unwrap().p95_ratio, Some(0.4));
        assert!(
            results[0]
                .reused_sample_ids
                .contains(&"reference-0-0".to_string())
        );
        assert!(
            results[2]
                .reused_sample_ids
                .contains(&"reference-0-0".to_string())
        );
    }

    #[test]
    fn reported_quantiles_handle_singleton_equal_and_outlier_samples() {
        for (name, elapsed, median, p95) in [
            ("singleton", vec![7], 7.0, 7.0),
            ("all equal", vec![7, 7, 7], 7.0, 7.0),
            // With eleven samples, p95 lies halfway between the last two.
            (
                "outlier",
                vec![1001, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
                1.0,
                501.0,
            ),
        ] {
            let observations: Vec<_> = elapsed
                .into_iter()
                .enumerate()
                .flat_map(|(repetition, elapsed)| {
                    [
                        sample(Variant::Reference, 0, repetition as u32, elapsed),
                        sample(Variant::ForkOn, 0, repetition as u32, elapsed * 2),
                    ]
                })
                .collect();
            let results = summarize(&observations);
            let result = &results[2];
            assert_eq!(result.baseline_distribution.median, Some(median), "{name}");
            assert_eq!(result.baseline_distribution.p95, Some(p95), "{name}");
            assert_eq!(
                result.candidate_distribution.median,
                Some(median * 2.0),
                "{name}"
            );
            assert_eq!(result.candidate_distribution.p95, Some(p95 * 2.0), "{name}");
            let observed = result.observed.as_ref().unwrap();
            assert_eq!(observed.median_difference, median, "{name}");
            assert_eq!(observed.p95_difference, p95, "{name}");
        }
    }

    #[test]
    fn failure_and_warmup_exclusions_do_not_shift_pairing() {
        let mut failed = sample(Variant::ForkOn, 0, 0, 999);
        failed.completed = false;
        let mut warmup = sample(Variant::Reference, 9, 0, 9_999);
        warmup.warmup = true;
        let results = summarize(&[
            sample(Variant::Reference, 0, 0, 100),
            failed,
            warmup,
            sample(Variant::ForkOn, 1, 0, 50),
        ]);
        let result = &results[2];
        assert_eq!(result.baseline_distribution.median, Some(100.0));
        assert_eq!(result.candidate_distribution.median, Some(50.0));
        assert!(result.pairs.is_empty());
        assert!(result.paired_observed.is_none());
        for (id, reason) in [
            ("fork_on-0-0", "not_completed"),
            ("reference-9-0", "warmup"),
            ("reference-0-0", "missing_completed_counterpart"),
        ] {
            assert!(
                result
                    .exclusions
                    .iter()
                    .any(|e| e.sample_id == id && e.reason == reason)
            );
        }
    }

    #[test]
    fn bootstrap_keeps_paired_values_together_across_clusters() {
        let observations: Vec<_> = [
            (0, 0, 100),
            (0, 1, 300),
            (1, 0, 600),
            (1, 1, 900),
            (2, 0, 600),
            (3, 0, 600),
            (4, 0, 600),
        ]
        .into_iter()
        .flat_map(|(cluster, repetition, elapsed)| {
            [
                sample(Variant::Reference, cluster, repetition, elapsed),
                sample(Variant::ForkOn, cluster, repetition, elapsed / 2),
            ]
        })
        .collect();
        let results = summarize(&observations);
        let result = &results[2];
        assert_eq!(result.baseline_distribution.median, Some(600.0));
        assert!((result.baseline_distribution.p95.unwrap() - 810.0).abs() < 1e-9);
        let bootstrap = result.bootstrap.as_ref().unwrap();
        assert_eq!(bootstrap.replicates, 10_000);
        assert_eq!(bootstrap.cluster_count, 5);
        assert_eq!(bootstrap.pair_count, 7);
        assert_eq!(
            bootstrap.median_ratio,
            Some(Interval {
                lower: 0.5,
                upper: 0.5
            })
        );
        assert_eq!(
            bootstrap.p95_ratio,
            Some(Interval {
                lower: 0.5,
                upper: 0.5
            })
        );
        let again = summarize(&observations);
        assert_eq!(
            bootstrap.median_difference,
            again[2].bootstrap.as_ref().unwrap().median_difference
        );
    }

    #[test]
    fn metrics_keep_their_own_coverage_pairing_and_fractional_values() {
        let mut observations = Vec::new();
        for cluster in 0..5 {
            for variant in [Variant::Reference, Variant::ForkOn] {
                let mut observation = sample(variant, cluster, 0, 100);
                let candidate = variant == Variant::ForkOn;
                observation.metrics.insert(
                    "first_output_ms".into(),
                    if candidate {
                        0.0
                    } else {
                        f64::from(cluster) + 0.5
                    },
                );
                observation.metrics.insert(
                    "scripted_serialized_request_bytes".into(),
                    if candidate { 20.0 } else { 40.0 },
                );
                if candidate && cluster == 3 {
                    observation.metrics.remove("first_output_ms");
                }
                if candidate && cluster == 4 {
                    observation
                        .metrics
                        .insert("first_output_ms".into(), f64::NAN);
                }
                observations.push(observation);
            }
        }
        let comparisons = summarize(&observations);
        let overall = |metric| {
            comparisons
                .iter()
                .find(|row| row.metric == metric && row.kind == "overall")
                .unwrap()
        };
        let elapsed = overall("elapsed_ms");
        assert_eq!(elapsed.pairs.len(), 5);
        assert!(elapsed.bootstrap.is_some());
        let first = overall("first_output_ms");
        assert_eq!(first.unit, "ms");
        assert_eq!(first.baseline_distribution.median, Some(2.5));
        assert_eq!(first.candidate_distribution.median, Some(0.0));
        assert_eq!(first.observed.as_ref().unwrap().median_difference, -2.5);
        assert_eq!(
            first.paired_observed.as_ref().unwrap().median_difference,
            -1.5
        );
        assert_eq!(
            first.paired_observed.as_ref().unwrap().median_ratio,
            Some(0.0)
        );
        assert_eq!(
            first
                .pairs
                .iter()
                .map(|pair| pair.cluster)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(first.cluster_count, 3);
        assert!(first.bootstrap.is_none());
        assert_eq!(
            first.interval_unavailable_reason.as_deref(),
            Some("insufficient_independent_run_clusters")
        );
        for (id, reason) in [
            ("fork_on-3-0", "metric_unavailable"),
            ("fork_on-4-0", "invalid_metric_value"),
        ] {
            assert!(
                first
                    .exclusions
                    .iter()
                    .any(|row| row.sample_id == id && row.reason == reason)
            );
        }
        let bytes = overall("scripted_serialized_request_bytes");
        assert_eq!(bytes.unit, "bytes");
        let bootstrap = bytes.bootstrap.as_ref().unwrap();
        assert_eq!(
            bootstrap.median_difference,
            Interval {
                lower: -20.0,
                upper: -20.0
            }
        );
        assert_ne!(bootstrap.seed, elapsed.bootstrap.as_ref().unwrap().seed);
        let saved = serde_json::to_value(first).unwrap();
        assert_eq!(
            saved["candidateDistribution"]["samples"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(saved["baselineDistribution"]["samples"][0]["value"], 0.5);
    }

    #[test]
    fn live_tasks_are_not_repetitions_and_never_receive_singleton_intervals() {
        let observations: Vec<_> = ["rust", "typescript", "python"]
            .into_iter()
            .flat_map(|task| {
                [Variant::Reference, Variant::ForkOff, Variant::ForkOn]
                    .into_iter()
                    .map(move |variant| {
                        let mut observation = sample(variant, 0, 0, 100);
                        observation.segment = Segment::RealModel;
                        observation.workload = task.to_string();
                        observation.id = format!("{task}-{}", variant.name());
                        observation
                    })
            })
            .collect();
        let results = summarize(&observations);
        assert_eq!(results.len(), 9);
        for result in results {
            assert_eq!(result.pairs.len(), 1);
            assert!(result.bootstrap.is_none());
            assert_eq!(
                result.interval_unavailable_reason.as_deref(),
                Some("insufficient_paired_repetitions")
            );
        }
    }

    #[test]
    fn intervals_require_five_paired_clusters_and_retain_exclusions() {
        let mut observations: Vec<_> = (0..5)
            .flat_map(|cluster| {
                let baseline = 100 + u64::from(cluster) * 100;
                [
                    sample(Variant::Reference, cluster, 0, baseline),
                    sample(Variant::ForkOn, cluster, 0, baseline + 20),
                ]
            })
            .collect();
        let results = summarize(&observations);
        let bootstrap = results[2].bootstrap.as_ref().unwrap();
        assert_eq!(bootstrap.cluster_count, 5);
        assert_eq!(bootstrap.pair_count, 5);
        assert_eq!(
            bootstrap.median_difference,
            Interval {
                lower: 20.0,
                upper: 20.0
            }
        );
        assert!((bootstrap.p95_difference.lower - 20.0).abs() < 1e-9);
        assert!((bootstrap.p95_difference.upper - 20.0).abs() < 1e-9);
        let three = summarize(&observations[..6]);
        assert_eq!(three[2].cluster_count, 3);
        assert!(three[2].bootstrap.is_none());
        assert_eq!(
            three[2].interval_unavailable_reason.as_deref(),
            Some("insufficient_independent_run_clusters")
        );
        for observation in &mut observations {
            if observation.variant == Variant::ForkOn && observation.cluster > 0 {
                observation.completed = false;
            }
        }
        let incomplete = summarize(&observations);
        assert_eq!(incomplete[2].pairs.len(), 1);
        assert!(incomplete[2].bootstrap.is_none());
        assert_eq!(
            incomplete[2].interval_unavailable_reason.as_deref(),
            Some("insufficient_paired_repetitions")
        );
        for id in ["fork_on-1-0", "fork_on-2-0"] {
            assert!(
                incomplete[2]
                    .exclusions
                    .iter()
                    .any(|sample| sample.sample_id == id && sample.reason == "not_completed")
            );
        }
    }

    #[test]
    fn ambiguous_slots_are_unpaired_and_missing_variants_are_reported() {
        let mut duplicate_slot = sample(Variant::ForkOn, 0, 0, 25);
        duplicate_slot.id = "distinct-attempt-same-slot".to_string();
        let results = summarize(&[
            sample(Variant::Reference, 0, 0, 100),
            sample(Variant::ForkOn, 0, 0, 50),
            duplicate_slot,
        ]);
        assert_eq!(results[0].missing_variants, vec![Variant::ForkOff]);
        assert!(results[2].pairs.is_empty());
        assert_eq!(results[2].exclusions.len(), 3);
        assert!(
            results[2]
                .exclusions
                .iter()
                .all(|e| e.reason == "ambiguous_schedule_slot")
        );
    }

    #[test]
    fn zero_baseline_preserves_differences_without_infinite_ratios() {
        let observations: Vec<_> = (0..5)
            .flat_map(|cluster| {
                [
                    sample(Variant::Reference, cluster, 0, 0),
                    sample(Variant::ForkOn, cluster, 0, 10),
                ]
            })
            .collect();
        let results = summarize(&observations);
        let result = &results[2];
        assert_eq!(result.observed.as_ref().unwrap().median_difference, 10.0);
        assert!(result.observed.as_ref().unwrap().median_ratio.is_none());
        let bootstrap = result.bootstrap.as_ref().unwrap();
        assert_eq!(
            bootstrap.median_difference,
            Interval {
                lower: 10.0,
                upper: 10.0
            }
        );
        assert!(bootstrap.median_ratio.is_none());
        let json = serde_json::to_value(result).unwrap();
        assert_eq!(json["bootstrap"]["medianRatio"], serde_json::Value::Null);
    }
}
