use serde::{Deserialize, Serialize};

pub const SCRIPTED_LIMIT_MS: u64 = 30 * 60 * 1000;
pub const ATTEMPT_LIMIT_MS: u64 = 10 * 60 * 1000;
pub const CLUSTERS: u32 = 14;
pub const WARMUPS: u32 = 3;
pub const ITERATIONS: u32 = 10;

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
            Self::Fast => 30,
            Self::Full => 90,
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

// Retained final-profile repetition counts; correctness scenarios have one
// measured observation per cluster. Both command modes use this exact schedule.
pub const SCRIPTED_WORKLOADS: [(&str, bool); 14] = [
    ("long_history_initial", false),
    ("long_history_continuation", false),
    ("stable_context_warm_cache", false),
    ("context_change_invalidation", false),
    ("direct_tools", false),
    ("parallel_tools", true),
    ("exclusive_tools", false),
    ("nested_tools", true),
    ("retained_process", false),
    ("abort_direct_nested", true),
    ("abort_retained", true),
    ("follow_up", true),
    ("restart_resume", true),
    ("cancel_then_prompt", true),
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

pub fn schedule(mode: Mode) -> Vec<ScheduledAttempt> {
    let mut attempts = Vec::new();
    for cluster in 0..CLUSTERS {
        for iteration in 0..WARMUPS + ITERATIONS {
            let warmup = iteration < WARMUPS;
            let repetition = if warmup {
                iteration
            } else {
                iteration - WARMUPS
            };
            for (index, (workload, correctness)) in SCRIPTED_WORKLOADS.iter().enumerate() {
                if *correctness && !warmup && repetition > 0 {
                    continue;
                }
                for variant in order(index + cluster as usize + iteration as usize) {
                    attempts.push(ScheduledAttempt {
                        id: format!(
                            "scripted-{workload}-c{cluster}-{}r{repetition}-{}",
                            if warmup { "warmup-" } else { "" },
                            variant.name()
                        ),
                        segment: Segment::Scripted,
                        workload: (*workload).into(),
                        variant,
                        cluster,
                        repetition,
                        warmup,
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
        let fast = schedule(Mode::Fast);
        let full = schedule(Mode::Full);
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
        assert_eq!(Mode::Fast.live_limit_ms(), 1_800_000);
        assert_eq!(Mode::Full.live_limit_ms(), 5_400_000);
    }
    #[test]
    fn all_scenarios_covered_before_repetition_and_counts_retained() {
        let schedule = schedule(Mode::Fast);
        let initial = &schedule[..SCRIPTED_WORKLOADS.len() * 3];
        for (name, _) in SCRIPTED_WORKLOADS {
            assert_eq!(initial.iter().filter(|s| s.workload == name).count(), 3);
        }
        for (name, correctness) in SCRIPTED_WORKLOADS {
            assert_eq!(
                schedule
                    .iter()
                    .filter(|s| s.workload == name && !s.warmup)
                    .count(),
                CLUSTERS as usize * 3 * if correctness { 1 } else { ITERATIONS as usize }
            );
        }
        let ids: std::collections::BTreeSet<_> = schedule.iter().map(|s| &s.id).collect();
        assert_eq!(ids.len(), schedule.len());
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
