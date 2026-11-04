//! Extend the existing turn clock/ledger; never hold its lock while polling.

use std::future::Future;
use std::task::Poll;

use codex_protocol::timing::TurnTimingBlockingCategory;
use codex_protocol::timing::TurnTimingBlockingEvidence;
use codex_protocol::timing::TurnTimingBlockingInterval;
use codex_protocol::timing::TurnTimingDependencyEdge;

use super::TurnTimingState;

const MAX_BLOCKING_INTERVALS: usize = 2_048;
const MAX_DEPENDENCIES: usize = 8_192;

pub(super) fn record_dependency(
    evidence: &mut TurnTimingBlockingEvidence,
    dependent: String,
    prerequisite: String,
    relation: &str,
) -> Option<TurnTimingDependencyEdge> {
    let edge = TurnTimingDependencyEdge { dependent, prerequisite, relation: relation.to_string() };
    if evidence.dependencies.len() < MAX_DEPENDENCIES {
        evidence.dependencies.push(edge);
        None
    } else {
        evidence.dropped_dependencies = evidence.dropped_dependencies.saturating_add(1);
        Some(edge)
    }
}

struct BlockingGuard<'a> {
    timing: &'a TurnTimingState,
    index: Option<usize>,
    overflow: Option<TurnTimingBlockingInterval>,
}

impl BlockingGuard<'_> {
    fn finish(&mut self, cancelled: bool) {
        let index = self.index.take();
        let overflow = self.overflow.take();
        if index.is_none() && overflow.is_none() { return; }
        let mut state = self.timing.state();
        // A terminal snapshot already froze this open interval as censored.
        if state.completed_snapshot.is_some() {
            return;
        }
        let now = self.timing.clock.sample().time.monotonic_ns;
        state.advance(now);
        let Some(end) = state.elapsed_since_start(now) else {
            return;
        };
        if let Some(interval) = index.and_then(|index| state
            .activity_timeline
            .blocking
            .as_mut()
            .and_then(|evidence| evidence.intervals.get_mut(index)))
        {
            interval.end_ns = Some(
                u64::try_from(end)
                    .unwrap_or(u64::MAX)
                    .max(interval.start_ns),
            );
            interval.cancelled = cancelled;
        }
        let record = overflow.and_then(|mut interval| {
            interval.end_ns = Some(u64::try_from(end).unwrap_or(u64::MAX).max(interval.start_ns));
            interval.cancelled = cancelled;
            state.archive_record(serde_json::json!({"kind":"blocking_interval", "interval":interval}), true)
        });
        drop(state);
        super::detail_archive::write(record);
    }
}

impl Drop for BlockingGuard<'_> {
    fn drop(&mut self) {
        self.finish(true);
    }
}

impl TurnTimingState {
    /// Observes only a future that actually suspends. No extra task, retry,
    /// admission attempt or lock acquisition is introduced around that future.
    pub async fn observe_wait<F: Future>(
        &self,
        dependent: &str,
        prerequisite: &str,
        category: TurnTimingBlockingCategory,
        future: F,
    ) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut guard = BlockingGuard {
            timing: self,
            index: None,
            overflow: None,
        };
        let mut observed = false;
        std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Ready(value) => {
                guard.finish(false);
                Poll::Ready(value)
            }
            Poll::Pending => {
                if !observed {
                    observed = true;
                    let mut state = self.state();
                    let now = self.clock.sample().time.monotonic_ns;
                    state.advance(now);
                    let mut dropped_edge = None;
                    let mut dropped_interval = None;
                    if state.completed_snapshot.is_none()
                        && let Some(start) = state.elapsed_since_start(now)
                        && let Some(evidence) = state.activity_timeline.blocking.as_mut()
                    {
                        if evidence.intervals.len() < MAX_BLOCKING_INTERVALS {
                            let index = evidence.intervals.len();
                            evidence.intervals.push(TurnTimingBlockingInterval {
                                id: index as u64,
                                dependent: dependent.to_string(),
                                prerequisite: prerequisite.to_string(),
                                category,
                                start_ns: u64::try_from(start).unwrap_or(u64::MAX),
                                end_ns: None,
                                cancelled: false,
                            });
                            dropped_edge = record_dependency(
                                evidence,
                                dependent.to_string(),
                                prerequisite.to_string(),
                                "awaits",
                            );
                            guard.index = Some(index);
                        } else {
                            // Its awaits edge is skipped with the interval.
                            evidence.dropped_intervals =
                                evidence.dropped_intervals.saturating_add(1);
                            evidence.dropped_dependencies =
                                evidence.dropped_dependencies.saturating_add(1);
                            let interval = TurnTimingBlockingInterval {
                                id: MAX_BLOCKING_INTERVALS as u64 + evidence.dropped_intervals - 1,
                                dependent: dependent.into(), prerequisite: prerequisite.into(), category,
                                start_ns: u64::try_from(start).unwrap_or(u64::MAX), end_ns: None, cancelled: false,
                            };
                            guard.overflow = Some(interval.clone());
                            dropped_interval = Some(interval);
                            dropped_edge = Some(TurnTimingDependencyEdge {
                                dependent: dependent.into(), prerequisite: prerequisite.into(), relation: "awaits".into(),
                            });
                        }
                    }
                    let edge_record = dropped_edge.and_then(|edge| state.archive_record(
                        serde_json::json!({"kind":"dependency", "edge":edge}), true));
                    let interval_record = dropped_interval.and_then(|interval| state.archive_record(
                        serde_json::json!({"kind":"blocking_interval", "interval":interval}), true));
                    drop(state);
                    super::detail_archive::write(edge_record);
                    super::detail_archive::write(interval_record);
                }
                Poll::Pending
            }
        })
        .await
    }
}
