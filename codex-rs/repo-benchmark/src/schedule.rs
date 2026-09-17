use serde::{Deserialize, Serialize};

pub const SCRIPTED_LIMIT_MS: u64 = 30 * 60 * 1000;
// Implementation plus focused validation can exceed the old ten-minute cap.
pub const ATTEMPT_LIMIT_MS: u64 = 20 * 60 * 1000;
// The native app-server boundary starts a fresh process for every attempt.
// Repeating the old in-process warmup/iteration matrix cannot warm later
// processes and exceeded the scripted budget. Freeze three independent
// measurements per workload/variant instead; actual runs establish budget fit.
pub const CLUSTERS: u32 = 3;
pub const WARMUPS: u32 = 0;
pub const ITERATIONS: u32 = 1;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Fast,
    Full,
}

impl Mode {
    pub fn live_limit_ms(self) -> u64 {
        (match self {
            // Cover all three variants at the per-attempt cap, with 25%
            // headroom for deadline overshoot and native teardown scheduling.
            Self::Fast => 75,
            Self::Full => 225,
        }) * 60
            * 1000
    }
    pub fn flag(self) -> &'static str {
        match self {
            Self::Fast => "-fast",
            Self::Full => "-full",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Variant {
    ForkOff,
    ForkOn,
    Reference,
}

impl Variant {
    pub const ALL: [Self; 3] = [Self::ForkOff, Self::ForkOn, Self::Reference];
    pub fn selected(fork_only_on: bool) -> impl Iterator<Item = Self> {
        Self::ALL
            .into_iter()
            .filter(move |variant| !fork_only_on || *variant != Self::ForkOff)
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::ForkOff => "fork_off",
            Self::ForkOn => "fork_on",
            Self::Reference => "reference",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Segment {
    Scripted,
    RealModel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledAttempt {
    pub id: String,
    pub segment: Segment,
    pub workload: String,
    pub variant: Variant,
    pub cluster: u32,
    pub repetition: u32,
    pub warmup: bool,
}

// Both modes use every workload. Measurements run through the native boundary;
// all workload/variant combinations precede the next independent cluster.
pub const SCRIPTED_WORKLOADS: [&str; 14] = [
    "long_history_initial",
    "long_history_continuation",
    "stable_context_warm_cache",
    "context_change_invalidation",
    "direct_tools",
    "parallel_tools",
    "exclusive_tools",
    "nested_tools",
    "retained_process",
    "abort_direct_nested",
    "abort_retained",
    "follow_up",
    "restart_resume",
    "cancel_then_prompt",
];

fn order(index: usize) -> [Variant; 3] {
    use Variant::*;
    [
        [ForkOff, ForkOn, Reference],
        [ForkOn, Reference, ForkOff],
        [Reference, ForkOff, ForkOn],
        [Reference, ForkOn, ForkOff],
        [ForkOn, ForkOff, Reference],
        [ForkOff, Reference, ForkOn],
    ][index % 6]
}

pub fn schedule(mode: Mode, fork_only_on: bool) -> Vec<ScheduledAttempt> {
    let mut attempts = Vec::new();
    for cluster in 0..CLUSTERS {
        for repetition in 0..ITERATIONS {
            for (index, workload) in SCRIPTED_WORKLOADS.iter().enumerate() {
                let mut variants = order(index + repetition as usize);
                // Balance every workload separately: each variant occupies each
                // position exactly once across the three independent clusters.
                let positions = variants.len();
                variants.rotate_left(cluster as usize % positions);
                for variant in variants {
                    attempts.push(ScheduledAttempt {
                        id: format!(
                            "scripted-{workload}-c{cluster}-r{repetition}-{}",
                            variant.name()
                        ),
                        segment: Segment::Scripted,
                        workload: (*workload).into(),
                        variant,
                        cluster,
                        repetition,
                        warmup: false,
                    });
                }
            }
        }
    }
    let tasks: &[&str] = match mode {
        Mode::Fast => &["rust_bugfix"],
        Mode::Full => &["rust_bugfix", "typescript_feature", "kd4_python_refactor"],
    };
    for (index, workload) in tasks.iter().enumerate() {
        for variant in order(index) {
            attempts.push(ScheduledAttempt {
                id: format!("real-{workload}-{}", variant.name()),
                segment: Segment::RealModel,
                workload: (*workload).into(),
                variant,
                cluster: 0,
                repetition: 0,
                warmup: false,
            });
        }
    }
    attempts.retain(|attempt| !fork_only_on || attempt.variant != Variant::ForkOff);
    attempts
}

#[derive(Debug)]
pub struct ExecutionBudget {
    limit_ms: u64,
    spent_ms: u64,
}

impl ExecutionBudget {
    pub fn new(limit_ms: u64) -> Self {
        Self {
            limit_ms,
            spent_ms: 0,
        }
    }
    pub fn remaining_ms(&self) -> u64 {
        self.limit_ms.saturating_sub(self.spent_ms)
    }
    pub fn charge(&mut self, elapsed_ms: u64) {
        self.spent_ms = self.spent_ms.saturating_add(elapsed_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modes_only_change_live_selection() {
        let fast = schedule(Mode::Fast, false);
        let full = schedule(Mode::Full, false);
        let scripted = |v: Vec<ScheduledAttempt>| {
            v.into_iter()
                .filter(|s| s.segment == Segment::Scripted)
                .collect::<Vec<_>>()
        };
        assert_eq!(scripted(fast.clone()), scripted(full.clone()));
        let live = |v: Vec<ScheduledAttempt>| {
            v.into_iter()
                .filter(|s| s.segment == Segment::RealModel)
                .collect::<Vec<_>>()
        };
        let fast_live = live(fast);
        assert_eq!(fast_live.len(), 3);
        assert!(fast_live.iter().all(|s| s.workload == "rust_bugfix"));
        assert_eq!(
            fast_live.iter().map(|s| s.variant).collect::<Vec<_>>(),
            Variant::ALL
        );
        assert_eq!(live(full).len(), 9);
        assert_eq!(Mode::Fast.live_limit_ms(), 4_500_000);
        assert_eq!(Mode::Full.live_limit_ms(), 13_500_000);
        for mode in [Mode::Fast, Mode::Full] {
            let attempts = schedule(mode, false)
                .iter()
                .filter(|attempt| attempt.segment == Segment::RealModel)
                .count() as u64;
            assert!(mode.live_limit_ms() >= attempts * ATTEMPT_LIMIT_MS);
        }
    }
    #[test]
    fn every_native_scenario_has_three_independent_measurements_without_warmups() {
        let schedule = schedule(Mode::Fast, false);
        let scripted: Vec<_> = schedule
            .iter()
            .filter(|attempt| attempt.segment == Segment::Scripted)
            .collect();
        // Independent contract expectations catch missing scenarios as well as
        // an accidental return to the old fresh-process repetition matrix.
        let expected = [
            "long_history_initial",
            "long_history_continuation",
            "stable_context_warm_cache",
            "context_change_invalidation",
            "direct_tools",
            "parallel_tools",
            "exclusive_tools",
            "nested_tools",
            "retained_process",
            "abort_direct_nested",
            "abort_retained",
            "follow_up",
            "restart_resume",
            "cancel_then_prompt",
        ];
        assert_eq!(scripted.len(), 126);
        assert_eq!(WARMUPS, 0);
        assert!(
            scripted
                .iter()
                .all(|attempt| !attempt.warmup && attempt.repetition == 0)
        );
        assert_eq!(SCRIPTED_LIMIT_MS, 1_800_000);
        for workload in expected {
            for variant in [Variant::ForkOff, Variant::ForkOn, Variant::Reference] {
                let clusters: Vec<_> = scripted
                    .iter()
                    .filter(|attempt| attempt.workload == workload && attempt.variant == variant)
                    .map(|attempt| attempt.cluster)
                    .collect();
                assert_eq!(clusters, [0, 1, 2], "{workload}/{}", variant.name());
                assert_eq!(scripted[..42].iter().filter(|attempt| attempt.workload == workload && attempt.variant == variant).count(), 1);
            }
        }
        assert!(
            schedule[..126]
                .iter()
                .all(|attempt| attempt.segment == Segment::Scripted)
        );
        assert!(
            schedule[126..]
                .iter()
                .all(|attempt| attempt.segment == Segment::RealModel)
        );
        let ids: std::collections::BTreeSet<_> = schedule.iter().map(|s| &s.id).collect();
        assert_eq!(ids.len(), schedule.len());
    }
    #[test]
    fn each_variant_occupies_every_native_execution_position_per_workload() {
        let schedule = schedule(Mode::Fast, false);
        for workload in SCRIPTED_WORKLOADS {
            let groups: Vec<_> = (0..3)
                .map(|cluster| {
                    schedule
                        .iter()
                        .filter(|attempt| {
                            attempt.segment == Segment::Scripted
                                && attempt.workload == workload
                                && attempt.cluster == cluster
                        })
                        .map(|attempt| attempt.variant)
                        .collect::<Vec<_>>()
                })
                .collect();
            for position in 0..3 {
                let variants: std::collections::BTreeSet<_> =
                    groups.iter().map(|group| group[position]).collect();
                assert_eq!(
                    variants,
                    [Variant::ForkOff, Variant::ForkOn, Variant::Reference]
                        .into_iter()
                        .collect(),
                    "{workload} position {position}"
                );
            }
        }
    }
    #[test]
    fn fork_only_on_preserves_original_measurement_ids_and_pairing() {
        for mode in [Mode::Fast, Mode::Full] {
            let selected = schedule(mode, true);
            let expected: Vec<_> = schedule(mode, false)
                .into_iter()
                .filter(|attempt| attempt.variant != Variant::ForkOff)
                .collect();
            assert_eq!(selected, expected);
            for workload in SCRIPTED_WORKLOADS {
                for variant in [Variant::ForkOn, Variant::Reference] {
                    let clusters: Vec<_> = selected
                        .iter()
                        .filter(|a| a.workload == workload && a.variant == variant)
                        .map(|a| a.cluster)
                        .collect();
                    assert_eq!(clusters, [0, 1, 2]);
                }
            }
        }
    }
    #[test]
    fn budgets_charge_only_execution_and_never_wrap() {
        let mut budget = ExecutionBudget::new(30);
        budget.charge(8);
        assert_eq!(budget.remaining_ms(), 22);
        budget.charge(50);
        assert_eq!(budget.remaining_ms(), 0);
    }
}
