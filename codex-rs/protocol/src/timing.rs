//! Same-process occurrence clock and bounded turn activity evidence.
//!
//! Offsets are comparable only when clock-domain IDs match. They are not Unix
//! timestamps, provider clocks, or evidence of task acceptance/client rendering.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

fn process_clock() -> &'static (String, Instant) {
    static CLOCK: OnceLock<(String, Instant)> = OnceLock::new();
    CLOCK.get_or_init(|| (uuid::Uuid::new_v4().to_string(), Instant::now()))
}

pub fn process_clock_domain_id() -> &'static str {
    &process_clock().0
}

pub fn process_monotonic_ns() -> u128 {
    process_clock().1.elapsed().as_nanos()
}

/// Convert a captured host instant into the existing process clock domain.
/// An observation predating that domain is unavailable, never clamped to zero.
pub fn process_monotonic_ns_at(observed_at: Instant) -> Option<u128> {
    observed_at
        .checked_duration_since(process_clock().1)
        .map(|value| value.as_nanos())
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingTimeline {
    pub version: u16,
    /// Absent for clocks without proven alignment (including legacy records).
    pub clock_domain_id: Option<String>,
    /// Turn start in the named process domain; intervals below are turn-relative.
    pub started_at_ns: u64,
    pub duration_ns: u64,
    pub intervals: Vec<TurnTimingActivityInterval>,
    /// Observed await suspensions and explicit dependency edges, not a complete
    /// task DAG. Absence is legacy unknown; idle time is never a blocking wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub blocking: Option<TurnTimingBlockingEvidence>,
    /// Complete, bounded suffix summaries after exact interval retention fills.
    /// Order inside a summary is unknown. Checkpoints carry the entire current
    /// suffix, replacing (not appending to) the preceding coarse snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coarse_intervals: Vec<TurnTimingActivitySummary>,
    pub dropped_intervals: u64,
    pub coverage_complete: bool,
    /// Complete elapsed coverage including coarse summaries, not exact ordering.
    /// Absent for legacy records; independent of exact `coverage_complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub coverage_including_coarse_complete: Option<bool>,
    /// Checkpoints do not assert a terminal endpoint.
    pub tail_unknown: bool,
    /// Processes still observed in flight at the frozen window end are censored.
    pub open_process_count: u32,
}

impl TurnTimingTimeline {
    /// Retain every overflow duration without unbounded memory or storage IO.
    /// Recent summaries span at least one second; pairwise compaction bounds
    /// the suffix to 128 summaries while preserving its partition and unions.
    pub fn record_coarse_interval(&mut self, interval: &TurnTimingActivityInterval) {
        const MAX_SUMMARIES: usize = 128;
        if self
            .coarse_intervals
            .last()
            .is_none_or(|last| last.end_ns.saturating_sub(last.start_ns) >= 1_000_000_000)
        {
            if self.coarse_intervals.len() >= MAX_SUMMARIES {
                let mut compacted = Vec::with_capacity(MAX_SUMMARIES);
                let mut previous = std::mem::take(&mut self.coarse_intervals).into_iter();
                while let Some(mut left) = previous.next() {
                    if let Some(right) = previous.next() {
                        left.end_ns = right.end_ns;
                        merge_durations(&mut left.exclusive_ns, right.exclusive_ns);
                        merge_durations(&mut left.unions_ns, right.unions_ns);
                    }
                    compacted.push(left);
                }
                self.coarse_intervals = compacted;
            }
            self.coarse_intervals.push(TurnTimingActivitySummary {
                start_ns: interval.start_ns,
                end_ns: interval.start_ns,
                ..Default::default()
            });
        }
        let Some(summary) = self.coarse_intervals.last_mut() else {
            return;
        };
        let elapsed = interval.end_ns.saturating_sub(interval.start_ns);
        summary.end_ns = interval.end_ns;
        let duration = summary
            .exclusive_ns
            .entry(interval.phase.clone())
            .or_default();
        *duration = duration.saturating_add(elapsed);
        for (name, active) in [
            ("modelActive", interval.model_active),
            ("toolActive", interval.tool_active),
            ("processInFlight", interval.process_in_flight),
            ("interactiveWait", interval.interactive_wait),
            ("compaction", interval.compaction),
            ("planning", interval.planning),
            (
                "modelProcessOverlap",
                interval.model_active && interval.process_in_flight,
            ),
            (
                "modelToolOverlap",
                interval.model_active && interval.tool_active,
            ),
        ] {
            if active {
                let duration = summary.unions_ns.entry(name.to_string()).or_default();
                *duration = duration.saturating_add(elapsed);
            }
        }
    }

    /// Recover a live local receipt only in this exact process clock domain.
    /// A persisted record from a restarted/remote process cannot become an Instant.
    pub fn local_end_instant(&self) -> Option<Instant> {
        if self.version != 1
            || self.tail_unknown
            || self.clock_domain_id.as_deref() != Some(process_clock_domain_id())
        {
            return None;
        }
        let end_ns = self.started_at_ns.checked_add(self.duration_ns)?;
        process_clock().1.checked_add(Duration::from_nanos(end_ns))
    }
}

fn merge_durations(target: &mut BTreeMap<String, u64>, source: BTreeMap<String, u64>) {
    for (name, elapsed) in source {
        let duration = target.entry(name).or_default();
        *duration = duration.saturating_add(elapsed);
    }
}

/// Bounded evidence for instrumented awaits only. Checkpoints upsert intervals
/// by ID and append edges; terminal records contain the complete retained lists.
/// Resource prerequisites name a resource class, not a holder or CPU scheduler.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingBlockingEvidence {
    pub version: u16,
    pub intervals: Vec<TurnTimingBlockingInterval>,
    pub dependencies: Vec<TurnTimingDependencyEdge>,
    pub dropped_intervals: u64,
    pub dropped_dependencies: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum TurnTimingBlockingCategory {
    ResourceContention,
    DependencyWait,
}

/// First Pending through Ready (or cancellation) of an awaited prerequisite.
/// Includes wake/re-poll latency, not synchronous work before the first Pending.
/// An absent end is right-censored at the enclosing timeline's frozen endpoint.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingBlockingInterval {
    pub id: u64,
    pub dependent: String,
    pub prerequisite: String,
    pub category: TurnTimingBlockingCategory,
    pub start_ns: u64,
    pub end_ns: Option<u64>,
    pub cancelled: bool,
}

/// Direction is dependent -> prerequisite. Generation and execution IDs are
/// scoped to this turn and clock domain; chronology alone never adds an edge.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingDependencyEdge {
    pub dependent: String,
    pub prerequisite: String,
    pub relation: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingActivitySummary {
    pub start_ns: u64,
    pub end_ns: u64,
    /// Canonical exclusive phase names to durations; sums to end minus start.
    pub exclusive_ns: BTreeMap<String, u64>,
    /// Union durations named after interval flags, plus modelProcessOverlap
    /// and modelToolOverlap. Missing keys are measured zero, not unknown.
    /// Values overlap and must not be added to the exclusive partition.
    pub unions_ns: BTreeMap<String, u64>,
}

/// Observed host queue costs, NOT provider compute or additive wall-clock time.
/// Sums include only enqueued/dequeued events; cancellation can censor a send
/// or leave events queued. Terminal receipt is independent of provider usage.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ResponseQueueTiming {
    pub enqueued_events: u64,
    pub dequeued_events: u64,
    pub enqueue_wait_ns: u64,
    pub queue_residence_ns: u64,
    pub max_enqueue_wait_ns: u64,
    pub max_queue_residence_ns: u64,
    pub terminal_received: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingActivityInterval {
    pub start_ns: u64,
    pub end_ns: u64,
    /// One of the ten canonical exclusive bucket names (without the Ns suffix).
    pub phase: String,
    pub model_active: bool,
    pub tool_active: bool,
    /// Observed process flight, NOT guarded tool runtime or CPU execution.
    pub process_in_flight: bool,
    pub interactive_wait: bool,
    pub compaction: bool,
    pub planning: bool,
}

