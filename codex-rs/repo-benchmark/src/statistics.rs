//! Measurements for the frozen schedule. No performance acceptance thresholds.
use std::collections::{BTreeMap, BTreeSet};

use rand::{Rng, SeedableRng, rngs::StdRng};
use serde::{Deserialize, Serialize};

use crate::schedule::{Segment, Variant};

pub const BOOTSTRAP_REPLICATES: usize = 10_000;
pub const BOOTSTRAP_SEED: u64 = 0x4b44_345f_4142_7631;

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
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Distribution {
    pub count: usize,
    /// Sorted by elapsed time; IDs stay attached to their measurements.
    pub samples: Vec<Sample>,
    pub median_ms: Option<f64>,
    pub p95_ms: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub id: String,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contrast {
    /// Candidate minus baseline, in milliseconds. Negative means less latency.
    pub median_difference_ms: f64,
    pub p95_difference_ms: f64,
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
    pub median_difference_ms: Interval,
    pub p95_difference_ms: Interval,
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
    pub exclusions: Vec<Exclusion>,
    pub missing_variants: Vec<Variant>,
    /// These are the same original observations in another comparison table.
    pub reused_sample_ids: Vec<String>,
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
        for (kind, baseline, candidate) in COMPARISONS {
            results.push(compare(
                segment, &workload, kind, baseline, candidate, &samples,
            ));
        }
    }
    results
}

fn compare(
    segment: Segment,
    workload: &str,
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
        } else {
            None
        };
        if let Some(reason) = reason {
            exclusions.push(exclude(sample, "completed_distribution", reason));
        } else {
            eligible.push(*sample);
        }
    }
    let baseline_distribution = distribution(&eligible, baseline);
    let candidate_distribution = distribution(&eligible, candidate);
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
                .push((a[0].elapsed_ms as f64, b[0].elapsed_ms as f64));
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
    // One run cannot estimate repeat-to-repeat uncertainty. For scripted data,
    // require more than one run cluster rather than treating within-run events
    // as independent runs. Live repetitions, when explicitly scheduled, may be
    // represented within a single task cluster.
    let interval_unavailable_reason = if pairs.len() < 2 {
        Some("insufficient_paired_repetitions".to_string())
    } else if segment == Segment::Scripted && clusters.len() < 2 {
        Some("insufficient_independent_run_clusters".to_string())
    } else {
        None
    };
    let bootstrap = if interval_unavailable_reason.is_none() {
        Some(bootstrap(&clusters))
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
    Comparison {
        segment,
        workload: workload.to_string(),
        kind: kind.to_string(),
        baseline,
        candidate,
        baseline_distribution,
        candidate_distribution,
        observed,
        paired_observed,
        pairs,
        exclusions,
        missing_variants,
        reused_sample_ids,
        bootstrap,
        interval_unavailable_reason,
        quantile_method: "sorted[ceil((n-1)*q)]".to_string(),
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

fn distribution(samples: &[&Observation], variant: Variant) -> Distribution {
    let mut samples: Vec<_> = samples
        .iter()
        .filter(|sample| sample.variant == variant)
        .map(|sample| Sample {
            id: sample.id.clone(),
            elapsed_ms: sample.elapsed_ms,
        })
        .collect();
    samples.sort_by(|a, b| a.elapsed_ms.cmp(&b.elapsed_ms).then(a.id.cmp(&b.id)));
    let values: Vec<_> = samples
        .iter()
        .map(|sample| sample.elapsed_ms as f64)
        .collect();
    Distribution {
        count: samples.len(),
        median_ms: percentile(&values, 0.5),
        p95_ms: percentile(&values, 0.95),
        samples,
    }
}

fn distribution_contrast(a: &Distribution, b: &Distribution) -> Option<Contrast> {
    Some(contrast_values(
        a.median_ms?,
        a.p95_ms?,
        b.median_ms?,
        b.p95_ms?,
    ))
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
        median_difference_ms: b_median - a_median,
        p95_difference_ms: b_p95 - a_p95,
        median_ratio: (a_median > 0.0).then(|| b_median / a_median),
        p95_ratio: (a_p95 > 0.0).then(|| b_p95 / a_p95),
    }
}

/// Preserve the previous runner's upper-order quantile convention.
fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    (!sorted.is_empty()).then(|| sorted[((sorted.len() - 1) as f64 * q).ceil() as usize])
}

fn interval(mut values: Vec<f64>) -> Interval {
    values.sort_by(f64::total_cmp);
    Interval {
        lower: values[((values.len() - 1) as f64 * 0.025).ceil() as usize],
        upper: values[((values.len() - 1) as f64 * 0.975).ceil() as usize],
    }
}

fn bootstrap(clusters: &[Vec<(f64, f64)>]) -> Bootstrap {
    let seed = b"elapsed_ms".iter().fold(BOOTSTRAP_SEED, |seed, byte| {
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
            median_differences.push(estimate.median_difference_ms);
            p95_differences.push(estimate.p95_difference_ms);
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
        median_difference_ms: interval(median_differences),
        p95_difference_ms: interval(p95_differences),
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
            elapsed_ms,
        }
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
            results[0].observed.as_ref().unwrap().median_difference_ms,
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
        assert_eq!(result.baseline_distribution.median_ms, Some(100.0));
        assert_eq!(result.candidate_distribution.median_ms, Some(50.0));
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
        let observations: Vec<_> = [(0, 0, 100), (0, 1, 300), (1, 0, 600), (1, 1, 900)]
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
        assert_eq!(result.baseline_distribution.median_ms, Some(600.0));
        assert_eq!(result.baseline_distribution.p95_ms, Some(900.0));
        let bootstrap = result.bootstrap.as_ref().unwrap();
        assert_eq!(bootstrap.replicates, 10_000);
        assert_eq!(bootstrap.cluster_count, 2);
        assert_eq!(bootstrap.pair_count, 4);
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
            bootstrap.median_difference_ms,
            again[2].bootstrap.as_ref().unwrap().median_difference_ms
        );
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
        let observations: Vec<_> = (0..2)
            .flat_map(|cluster| {
                [
                    sample(Variant::Reference, cluster, 0, 0),
                    sample(Variant::ForkOn, cluster, 0, 10),
                ]
            })
            .collect();
        let results = summarize(&observations);
        let result = &results[2];
        assert_eq!(result.observed.as_ref().unwrap().median_difference_ms, 10.0);
        assert!(result.observed.as_ref().unwrap().median_ratio.is_none());
        let bootstrap = result.bootstrap.as_ref().unwrap();
        assert_eq!(
            bootstrap.median_difference_ms,
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
