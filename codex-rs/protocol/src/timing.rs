//! Same-process occurrence clock and bounded turn activity evidence.
//!
//! Offsets are comparable only when clock-domain IDs match. They are not Unix
//! timestamps, provider clocks, or evidence of task acceptance/client rendering.

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
    pub dropped_intervals: u64,
    pub coverage_complete: bool,
    /// Checkpoints do not assert a terminal endpoint.
    pub tail_unknown: bool,
    /// Processes still observed in flight at the frozen window end are censored.
    pub open_process_count: u32,
}

impl TurnTimingTimeline {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_same_domain_closed_receipts_recover_local_instants() {
        let mut timeline = TurnTimingTimeline {
            version: 1,
            clock_domain_id: Some(process_clock_domain_id().to_string()),
            started_at_ns: u64::try_from(process_monotonic_ns()).unwrap(),
            ..Default::default()
        };
        assert!(timeline.local_end_instant().is_some());
        timeline.tail_unknown = true;
        assert!(timeline.local_end_instant().is_none());
        timeline.tail_unknown = false;
        timeline.clock_domain_id = Some("other-process".to_string());
        assert!(timeline.local_end_instant().is_none());
        timeline.clock_domain_id = None;
        assert!(timeline.local_end_instant().is_none());
    }
}
