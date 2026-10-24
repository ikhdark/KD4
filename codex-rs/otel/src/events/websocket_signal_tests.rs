use super::*;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use tracing_subscriber::Layer as _;

#[test]
fn websocket_timing_signal_matrix_preserves_independent_receipts() {
    for metrics_enabled in [false, true] {
        for logs_enabled in [false, true] {
            for traces_enabled in [false, true] {
                let logs = InMemoryLogExporter::default();
                let logger = SdkLoggerProvider::builder().with_simple_exporter(logs.clone()).build();
                let spans = InMemorySpanExporter::default();
                let tracer = SdkTracerProvider::builder().with_simple_exporter(spans.clone()).build();
                let subscriber = tracing_subscriber::registry()
                    .with(opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&logger)
                        .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| logs_enabled && OtelProvider::log_export_filter(metadata))))
                    .with(tracing_opentelemetry::layer().with_tracer(tracer.tracer("websocket-signals"))
                        .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| traces_enabled && OtelProvider::trace_export_filter(metadata))));
                let mut manager = telemetry();
                manager.metrics = metrics_enabled.then(|| MetricsClient::new(
                    MetricsConfig::in_memory("test", "test", "1", InMemoryMetricExporter::default()).with_runtime_reader()
                ).unwrap());
                tracing::subscriber::with_default(subscriber, || {
                    tracing::callsite::rebuild_interest_cache();
                    let span = tracing::info_span!("websocket-signal-test");
                    let _entered = span.enter();
                    let frame = Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(
                        r#"{"type":"responsesapi.websocket_timing","timing_metrics":{"engine_service_total_ms":12.5}}"#.into()
                    ))));
                    manager.record_websocket_event_with_transport(&frame, Duration::from_millis(7), Some("request-1"));
                });
                logger.force_flush().unwrap();
                tracer.force_flush().unwrap();
                let emitted = logs.get_emitted_logs().unwrap();
                assert_eq!(emitted.len(), usize::from(logs_enabled));
                if logs_enabled {
                    let attributes = emitted[0].record.attributes_iter().map(|(key, value)| (key.as_str().to_owned(), value.clone())).collect::<BTreeMap<_, _>>();
                    assert_eq!(attributes["event.name"], opentelemetry::logs::AnyValue::String("codex.provider_websocket_timing".into()));
                    assert_eq!(attributes["engine_service_total_ms"], opentelemetry::logs::AnyValue::Double(12.5));
                    assert_eq!(attributes["logical_request_id"], opentelemetry::logs::AnyValue::String("request-1".into()));
                }
                let spans = spans.get_finished_spans().unwrap();
                let receipts = spans.iter().flat_map(|span| span.events.iter()).filter(|event|
                    event.attributes.iter().any(|value| value.key.as_str() == "event.name" && value.value.as_str() == "codex.provider_websocket_timing")
                ).count();
                assert_eq!(receipts, usize::from(traces_enabled));
                if metrics_enabled {
                    let summary = manager.runtime_metrics_summary().unwrap();
                    assert_eq!(summary.websocket_events.count, 1);
                    assert_eq!(summary.websocket_events.duration_ms, 7);
                } else {
                    assert!(manager.snapshot_metrics().is_err());
                }
                manager.shutdown_metrics().unwrap();
            }
        }
    }
}

#[test]
fn websocket_error_envelopes_record_failed_count_and_duration_once() {
    use tokio_tungstenite::tungstenite::Message;
    for (message, expected) in [
        (Message::Text(r#"{"type":"error"}"#.into()), Some(("error", "false"))),
        (Message::Text(r#"{"type":"response.failed"}"#.into()), Some(("response.failed", "false"))),
        (Message::Text(r#"{"type":"response.output_text.delta"}"#.into()), Some(("response.output_text.delta", "true"))),
        (Message::Text("{".into()), Some(("parse_error", "false"))),
        (Message::Ping(Vec::new().into()), None),
        (Message::Pong(Vec::new().into()), None),
    ] {
        let manager = telemetry().with_metrics(MetricsClient::new(
            MetricsConfig::in_memory("test", "test", "1", InMemoryMetricExporter::default()).with_runtime_reader()
        ).unwrap());
        manager.record_websocket_event(&Ok(Some(Ok(message))), Duration::from_millis(7));
        let snapshot = manager.snapshot_metrics().unwrap();
        for name in [WEBSOCKET_EVENT_COUNT_METRIC, WEBSOCKET_EVENT_DURATION_METRIC] {
            let metric = snapshot.scope_metrics().flat_map(|scope| scope.metrics()).find(|metric| metric.name() == name);
            let Some((kind, success)) = expected else { assert!(metric.is_none()); continue; };
            let metric = metric.unwrap();
            let attributes = match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                    let points = sum.data_points().collect::<Vec<_>>();
                    assert_eq!(points.len(), 1);
                    assert_eq!(points[0].value(), 1);
                    points[0].attributes().cloned().collect::<Vec<_>>()
                }
                AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                    let points = histogram.data_points().collect::<Vec<_>>();
                    assert_eq!(points.len(), 1);
                    assert_eq!(points[0].count(), 1);
                    assert_eq!(points[0].sum(), 7.0);
                    points[0].attributes().cloned().collect::<Vec<_>>()
                }
                other => panic!("unexpected metric: {other:?}"),
            };
            assert!(attributes.iter().any(|value| value.key.as_str() == "kind" && value.value.as_str() == kind));
            assert!(attributes.iter().any(|value| value.key.as_str() == "success" && value.value.as_str() == success));
        }
        manager.shutdown_metrics().unwrap();
    }
}
