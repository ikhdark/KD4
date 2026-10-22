use crate::metrics::names::API_CALL_COUNT_METRIC;
use crate::metrics::names::API_CALL_DURATION_METRIC;
use crate::metrics::names::RESPONSES_API_ENGINE_IAPI_TBT_DURATION_METRIC;
use crate::metrics::names::RESPONSES_API_ENGINE_IAPI_TTFT_DURATION_METRIC;
use crate::metrics::names::RESPONSES_API_ENGINE_SERVICE_TBT_DURATION_METRIC;
use crate::metrics::names::RESPONSES_API_ENGINE_SERVICE_TTFT_DURATION_METRIC;
use crate::metrics::names::RESPONSES_API_INFERENCE_TIME_DURATION_METRIC;
use crate::metrics::names::RESPONSES_API_OVERHEAD_DURATION_METRIC;
use crate::metrics::names::SSE_EVENT_COUNT_METRIC;
use crate::metrics::names::SSE_EVENT_DURATION_METRIC;
use crate::metrics::names::TOOL_CALL_COUNT_METRIC;
use crate::metrics::names::TOOL_CALL_DURATION_METRIC;
use crate::metrics::names::TURN_TTFM_DURATION_METRIC;
use crate::metrics::names::TURN_TTFT_DURATION_METRIC;
use crate::metrics::names::WEBSOCKET_EVENT_COUNT_METRIC;
use crate::metrics::names::WEBSOCKET_EVENT_DURATION_METRIC;
use crate::metrics::names::WEBSOCKET_REQUEST_COUNT_METRIC;
use crate::metrics::names::WEBSOCKET_REQUEST_DURATION_METRIC;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::Metric;
use opentelemetry_sdk::metrics::data::MetricData;
use opentelemetry_sdk::metrics::data::ResourceMetrics;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeMetricTotals {
    pub count: u64,
    pub duration_ms: u64,
}

impl RuntimeMetricTotals {
    pub fn is_empty(self) -> bool {
        self.count == 0 && self.duration_ms == 0
    }

    pub fn merge(&mut self, other: Self) {
        self.count = self.count.saturating_add(other.count);
        self.duration_ms = self.duration_ms.saturating_add(other.duration_ms);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeMetricsSummary {
    pub tool_calls: RuntimeMetricTotals,
    pub api_calls: RuntimeMetricTotals,
    pub streaming_events: RuntimeMetricTotals,
    pub websocket_calls: RuntimeMetricTotals,
    pub websocket_events: RuntimeMetricTotals,
    pub responses_api_overhead_ms: u64,
    pub responses_api_inference_time_ms: u64,
    pub responses_api_engine_iapi_ttft_ms: u64,
    pub responses_api_engine_service_ttft_ms: u64,
    pub responses_api_engine_iapi_tbt_ms: u64,
    pub responses_api_engine_service_tbt_ms: u64,
    pub turn_ttft_ms: u64,
    pub turn_ttfm_ms: u64,
    /// Sub-millisecond carry in the same order as the thirteen duration fields.
    /// This keeps poll boundaries from rounding away short observations.
    #[doc(hidden)]
    pub duration_residual_ns: [i32; 13],
}

impl RuntimeMetricsSummary {
    pub fn is_empty(self) -> bool {
        self.tool_calls.is_empty()
            && self.api_calls.is_empty()
            && self.streaming_events.is_empty()
            && self.websocket_calls.is_empty()
            && self.websocket_events.is_empty()
            && self.responses_api_overhead_ms == 0
            && self.responses_api_inference_time_ms == 0
            && self.responses_api_engine_iapi_ttft_ms == 0
            && self.responses_api_engine_service_ttft_ms == 0
            && self.responses_api_engine_iapi_tbt_ms == 0
            && self.responses_api_engine_service_tbt_ms == 0
            && self.turn_ttft_ms == 0
            && self.turn_ttfm_ms == 0
            && self.duration_residual_ns.iter().all(|value| *value == 0)
    }

    /// Accumulate process-local histogram observation sums. These are neither
    /// a latest-response vector nor exclusive turn/inference durations.
    pub fn merge(&mut self, other: Self) {
        for (index, (left, right)) in [
            (&mut self.tool_calls, other.tool_calls),
            (&mut self.api_calls, other.api_calls),
            (&mut self.streaming_events, other.streaming_events),
            (&mut self.websocket_calls, other.websocket_calls),
            (&mut self.websocket_events, other.websocket_events),
        ]
        .into_iter()
        .enumerate()
        {
            left.count = left.count.saturating_add(right.count);
            merge_duration_ms(
                &mut left.duration_ms,
                &mut self.duration_residual_ns[index],
                right.duration_ms,
                other.duration_residual_ns[index],
            );
        }
        for (offset, (left, right)) in [
            (
                &mut self.responses_api_overhead_ms,
                other.responses_api_overhead_ms,
            ),
            (
                &mut self.responses_api_inference_time_ms,
                other.responses_api_inference_time_ms,
            ),
            (
                &mut self.responses_api_engine_iapi_ttft_ms,
                other.responses_api_engine_iapi_ttft_ms,
            ),
            (
                &mut self.responses_api_engine_service_ttft_ms,
                other.responses_api_engine_service_ttft_ms,
            ),
            (
                &mut self.responses_api_engine_iapi_tbt_ms,
                other.responses_api_engine_iapi_tbt_ms,
            ),
            (
                &mut self.responses_api_engine_service_tbt_ms,
                other.responses_api_engine_service_tbt_ms,
            ),
            (&mut self.turn_ttft_ms, other.turn_ttft_ms),
            (&mut self.turn_ttfm_ms, other.turn_ttfm_ms),
        ]
        .into_iter()
        .enumerate()
        {
            let index = offset + 5;
            merge_duration_ms(
                left,
                &mut self.duration_residual_ns[index],
                right,
                other.duration_residual_ns[index],
            );
        }
    }

    pub fn responses_api_summary(&self) -> RuntimeMetricsSummary {
        Self {
            responses_api_overhead_ms: self.responses_api_overhead_ms,
            responses_api_inference_time_ms: self.responses_api_inference_time_ms,
            responses_api_engine_iapi_ttft_ms: self.responses_api_engine_iapi_ttft_ms,
            responses_api_engine_service_ttft_ms: self.responses_api_engine_service_ttft_ms,
            responses_api_engine_iapi_tbt_ms: self.responses_api_engine_iapi_tbt_ms,
            responses_api_engine_service_tbt_ms: self.responses_api_engine_service_tbt_ms,
            duration_residual_ns: std::array::from_fn(|index| {
                if (5..11).contains(&index) {
                    self.duration_residual_ns[index]
                } else {
                    0
                }
            }),
            ..RuntimeMetricsSummary::default()
        }
    }

    pub(crate) fn from_snapshot(snapshot: &ResourceMetrics) -> Self {
        let mut summary = Self::default();
        // Accumulate fractional milliseconds across all attribute points and scopes
        // and retain fractional carry across polling windows.
        let mut durations = [0.0; 13];
        for metric in snapshot
            .scope_metrics()
            .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        {
            let counter = match metric.name() {
                TOOL_CALL_COUNT_METRIC => Some(&mut summary.tool_calls.count),
                API_CALL_COUNT_METRIC => Some(&mut summary.api_calls.count),
                SSE_EVENT_COUNT_METRIC => Some(&mut summary.streaming_events.count),
                WEBSOCKET_REQUEST_COUNT_METRIC => Some(&mut summary.websocket_calls.count),
                WEBSOCKET_EVENT_COUNT_METRIC => Some(&mut summary.websocket_events.count),
                _ => None,
            };
            if let Some(counter) = counter {
                *counter = counter.saturating_add(sum_counter_metric(metric));
                continue;
            }
            let duration = match metric.name() {
                TOOL_CALL_DURATION_METRIC => &mut durations[0],
                API_CALL_DURATION_METRIC => &mut durations[1],
                SSE_EVENT_DURATION_METRIC => &mut durations[2],
                WEBSOCKET_REQUEST_DURATION_METRIC => &mut durations[3],
                WEBSOCKET_EVENT_DURATION_METRIC => &mut durations[4],
                RESPONSES_API_OVERHEAD_DURATION_METRIC => &mut durations[5],
                RESPONSES_API_INFERENCE_TIME_DURATION_METRIC => &mut durations[6],
                RESPONSES_API_ENGINE_IAPI_TTFT_DURATION_METRIC => &mut durations[7],
                RESPONSES_API_ENGINE_SERVICE_TTFT_DURATION_METRIC => &mut durations[8],
                RESPONSES_API_ENGINE_IAPI_TBT_DURATION_METRIC => &mut durations[9],
                RESPONSES_API_ENGINE_SERVICE_TBT_DURATION_METRIC => &mut durations[10],
                TURN_TTFT_DURATION_METRIC => &mut durations[11],
                TURN_TTFM_DURATION_METRIC => &mut durations[12],
                _ => continue,
            };
            if let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data() {
                for point in histogram.data_points() {
                    let value = point.sum();
                    if value.is_finite() && value > 0.0 {
                        *duration = (*duration + value).min(u64::MAX as f64);
                    }
                }
            }
        }
        let rounded =
            durations.map(|value| round_duration_ns((value * 1_000_000.0).round() as u128));
        summary.tool_calls.duration_ms = rounded[0].0;
        summary.api_calls.duration_ms = rounded[1].0;
        summary.streaming_events.duration_ms = rounded[2].0;
        summary.websocket_calls.duration_ms = rounded[3].0;
        summary.websocket_events.duration_ms = rounded[4].0;
        summary.responses_api_overhead_ms = rounded[5].0;
        summary.responses_api_inference_time_ms = rounded[6].0;
        summary.responses_api_engine_iapi_ttft_ms = rounded[7].0;
        summary.responses_api_engine_service_ttft_ms = rounded[8].0;
        summary.responses_api_engine_iapi_tbt_ms = rounded[9].0;
        summary.responses_api_engine_service_tbt_ms = rounded[10].0;
        summary.turn_ttft_ms = rounded[11].0;
        summary.turn_ttfm_ms = rounded[12].0;
        summary.duration_residual_ns = rounded.map(|(_, residual)| residual);
        summary
    }
}

fn sum_counter_metric(metric: &Metric) -> u64 {
    match metric.data() {
        AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
            .data_points()
            .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
            .fold(0, u64::saturating_add),
        _ => 0,
    }
}

fn round_duration_ns(value: u128) -> (u64, i32) {
    let rounded_ms = value / 1_000_000 + u128::from(value % 1_000_000 >= 500_000);
    if rounded_ms > u128::from(u64::MAX) {
        return (u64::MAX, 0);
    }
    (
        rounded_ms as u64,
        (value as i128 - rounded_ms as i128 * 1_000_000) as i32,
    )
}

fn merge_duration_ms(total: &mut u64, residual: &mut i32, other: u64, other_residual: i32) {
    let nanos = (i128::from(*total) + i128::from(other)) * 1_000_000
        + i128::from(*residual)
        + i128::from(other_residual);
    (*total, *residual) = round_duration_ns(nanos.max(0) as u128);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MetricsClient;
    use crate::MetricsConfig;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use std::time::Duration;

    #[test]
    fn audit_observability_runtime_poll_partition_uses_observation_sums() {
        let first = RuntimeMetricsSummary {
            responses_api_engine_iapi_ttft_ms: 120,
            responses_api_engine_service_tbt_ms: 50,
            ..Default::default()
        };
        let second = RuntimeMetricsSummary {
            responses_api_engine_iapi_ttft_ms: 80,
            ..Default::default()
        };
        let mut separate = first;
        separate.merge(second);
        let mut together = RuntimeMetricsSummary::default();
        together.merge(RuntimeMetricsSummary {
            responses_api_engine_iapi_ttft_ms: 200,
            responses_api_engine_service_tbt_ms: 50,
            ..Default::default()
        });
        assert_eq!(separate, together);
        separate.merge(RuntimeMetricsSummary::default());
        assert_eq!(separate, together);
    }

    #[test]
    fn audit_observability_fractional_windows_match_one_collection() {
        let metrics = MetricsClient::new(
            MetricsConfig::in_memory("test", "test", "1", InMemoryMetricExporter::default())
                .with_runtime_reader(),
        )
        .unwrap();
        let record = || {
            metrics
                .record_count_and_duration(
                    API_CALL_COUNT_METRIC,
                    RESPONSES_API_ENGINE_IAPI_TTFT_DURATION_METRIC,
                    1,
                    Duration::from_micros(400),
                    &[],
                )
                .unwrap()
        };
        record();
        let mut split = RuntimeMetricsSummary::from_snapshot(&metrics.snapshot().unwrap());
        record();
        split.merge(RuntimeMetricsSummary::from_snapshot(
            &metrics.snapshot().unwrap(),
        ));
        record();
        record();
        let combined = RuntimeMetricsSummary::from_snapshot(&metrics.snapshot().unwrap());
        assert_eq!(split, combined);
        assert_eq!(split.responses_api_engine_iapi_ttft_ms, 1); // 800 us rounded once
        assert_eq!(split.duration_residual_ns[7], -200_000);
        split.merge(RuntimeMetricsSummary::default());
        assert_eq!(split, combined);
        metrics.shutdown().unwrap();
    }

    #[test]
    fn snapshot_sums_fractional_attribute_points_before_rounding_and_saturates_counts() {
        let metrics = MetricsClient::new(
            MetricsConfig::in_memory("test", "test", "1", InMemoryMetricExporter::default())
                .with_runtime_reader(),
        )
        .unwrap();
        for tag in ["one", "two", "three"] {
            metrics
                .record_count_and_duration(
                    TOOL_CALL_COUNT_METRIC,
                    TOOL_CALL_DURATION_METRIC,
                    i64::MAX,
                    Duration::from_micros(400),
                    &[("part", tag)],
                )
                .unwrap();
        }
        let summary = RuntimeMetricsSummary::from_snapshot(&metrics.snapshot().unwrap());
        assert_eq!(
            summary.tool_calls,
            RuntimeMetricTotals {
                count: u64::MAX,
                duration_ms: 1
            }
        );
        assert_eq!(summary.api_calls, RuntimeMetricTotals::default());
        assert_eq!(
            RuntimeMetricsSummary::from_snapshot(&metrics.snapshot().unwrap()),
            RuntimeMetricsSummary::default()
        );
        metrics.shutdown().unwrap();
    }
}
