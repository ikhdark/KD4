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
    }

    /// Accumulate activity totals while retaining the latest nonzero collection
    /// window for server and first-response timings displayed by the TUI.
    pub fn merge(&mut self, other: Self) {
        self.tool_calls.merge(other.tool_calls);
        self.api_calls.merge(other.api_calls);
        self.streaming_events.merge(other.streaming_events);
        self.websocket_calls.merge(other.websocket_calls);
        self.websocket_events.merge(other.websocket_events);
        if other.responses_api_overhead_ms > 0 {
            self.responses_api_overhead_ms = other.responses_api_overhead_ms;
        }
        if other.responses_api_inference_time_ms > 0 {
            self.responses_api_inference_time_ms = other.responses_api_inference_time_ms;
        }
        if other.responses_api_engine_iapi_ttft_ms > 0 {
            self.responses_api_engine_iapi_ttft_ms = other.responses_api_engine_iapi_ttft_ms;
        }
        if other.responses_api_engine_service_ttft_ms > 0 {
            self.responses_api_engine_service_ttft_ms = other.responses_api_engine_service_ttft_ms;
        }
        if other.responses_api_engine_iapi_tbt_ms > 0 {
            self.responses_api_engine_iapi_tbt_ms = other.responses_api_engine_iapi_tbt_ms;
        }
        if other.responses_api_engine_service_tbt_ms > 0 {
            self.responses_api_engine_service_tbt_ms = other.responses_api_engine_service_tbt_ms;
        }
        if other.turn_ttft_ms > 0 {
            self.turn_ttft_ms = other.turn_ttft_ms;
        }
        if other.turn_ttfm_ms > 0 {
            self.turn_ttfm_ms = other.turn_ttfm_ms;
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
            ..RuntimeMetricsSummary::default()
        }
    }

    pub(crate) fn from_snapshot(snapshot: &ResourceMetrics) -> Self {
        let mut summary = Self::default();
        // Accumulate fractional milliseconds across all attribute points and scopes
        // before rounding at the public integer-summary boundary.
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
        summary.tool_calls.duration_ms = f64_to_u64(durations[0]);
        summary.api_calls.duration_ms = f64_to_u64(durations[1]);
        summary.streaming_events.duration_ms = f64_to_u64(durations[2]);
        summary.websocket_calls.duration_ms = f64_to_u64(durations[3]);
        summary.websocket_events.duration_ms = f64_to_u64(durations[4]);
        summary.responses_api_overhead_ms = f64_to_u64(durations[5]);
        summary.responses_api_inference_time_ms = f64_to_u64(durations[6]);
        summary.responses_api_engine_iapi_ttft_ms = f64_to_u64(durations[7]);
        summary.responses_api_engine_service_ttft_ms = f64_to_u64(durations[8]);
        summary.responses_api_engine_iapi_tbt_ms = f64_to_u64(durations[9]);
        summary.responses_api_engine_service_tbt_ms = f64_to_u64(durations[10]);
        summary.turn_ttft_ms = f64_to_u64(durations[11]);
        summary.turn_ttfm_ms = f64_to_u64(durations[12]);
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

fn f64_to_u64(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    let clamped = value.min(u64::MAX as f64);
    clamped.round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MetricsClient;
    use crate::MetricsConfig;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use std::time::Duration;

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
