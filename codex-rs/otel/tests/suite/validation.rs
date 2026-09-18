use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::MetricsError;
use codex_otel::Result;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;

fn build_in_memory_client() -> Result<MetricsClient> {
    let exporter = InMemoryMetricExporter::default();
    let config = MetricsConfig::in_memory("test", "codex-cli", env!("CARGO_PKG_VERSION"), exporter);
    MetricsClient::new(config)
}

// Ensures invalid tag components are rejected during config build.
#[test]
fn invalid_tag_component_is_rejected() -> Result<()> {
    let err = MetricsConfig::in_memory(
        "test",
        "codex-cli",
        env!("CARGO_PKG_VERSION"),
        InMemoryMetricExporter::default(),
    )
    .with_tag("bad key", "value")
    .unwrap_err();
    assert!(matches!(
        err,
        MetricsError::InvalidTagComponent { label, value }
            if label == "tag key" && value == "bad key"
    ));
    Ok(())
}

// Ensures per-metric tag keys are validated.
#[test]
fn counter_rejects_invalid_tag_key() -> Result<()> {
    let metrics = build_in_memory_client()?;
    let err = metrics
        .counter("codex.turns", /*inc*/ 1, &[("bad key", "value")])
        .unwrap_err();
    assert!(matches!(
        err,
        MetricsError::InvalidTagComponent { label, value }
            if label == "tag key" && value == "bad key"
    ));
    metrics.shutdown()?;
    Ok(())
}

// Ensures per-metric tag values are validated.
#[test]
fn histogram_rejects_invalid_tag_value() -> Result<()> {
    let metrics = build_in_memory_client()?;
    let err = metrics
        .histogram(
            "codex.request_latency",
            /*value*/ 3,
            &[("route", "bad value")],
        )
        .unwrap_err();
    assert!(matches!(
        err,
        MetricsError::InvalidTagComponent { label, value }
            if label == "tag value" && value == "bad value"
    ));
    metrics.shutdown()?;
    Ok(())
}

// Ensures invalid metric names are rejected.
#[test]
fn counter_rejects_invalid_metric_name() -> Result<()> {
    let metrics = build_in_memory_client()?;
    let err = metrics.counter("bad name", 1, &[]).unwrap_err();
    assert!(matches!(err, MetricsError::InvalidMetricName { name } if name == "bad name"));
    metrics.shutdown()?;
    Ok(())
}

#[test]
fn counter_rejects_negative_increment() -> Result<()> {
    let metrics = build_in_memory_client()?;
    let err = metrics.counter("codex.turns", /*inc*/ -1, &[]).unwrap_err();
    assert!(matches!(
        err,
        MetricsError::NegativeCounterIncrement { name, inc } if name == "codex.turns" && inc == -1
    ));
    metrics.shutdown()?;
    Ok(())
}

#[test]
fn metric_names_enforce_sdk_prefix_and_length() -> Result<()> {
    let metrics = build_in_memory_client()?;
    for name in ["1bad".to_string(), "-".to_string(), "a".repeat(256)] {
        assert!(matches!(
            metrics.counter(&name, 1, &[]),
            Err(MetricsError::InvalidMetricName { .. })
        ));
    }
    metrics.counter(&"a".repeat(255), 1, &[])?;
    metrics.shutdown()?;
    Ok(())
}

#[test]
fn timer_rejects_invalid_inputs_at_construction() -> Result<()> {
    let metrics = build_in_memory_client()?;
    assert!(matches!(
        metrics.start_timer("1bad", &[]),
        Err(MetricsError::InvalidMetricName { .. })
    ));
    assert!(matches!(
        metrics.start_timer("codex.timer", &[("bad key", "value")]),
        Err(MetricsError::InvalidTagComponent { .. })
    ));
    assert!(matches!(
        metrics.start_timer("codex.timer", &[("key", "bad value")]),
        Err(MetricsError::InvalidTagComponent { .. })
    ));
    metrics.shutdown()?;
    Ok(())
}

#[test]
fn grpc_exporters_reject_invalid_and_ambiguous_headers_without_credentials_in_errors() {
    use codex_otel::OtelExporter;
    use codex_otel::OtelProvider;
    use codex_otel::OtelSettings;
    use std::collections::BTreeMap;
    use std::collections::HashMap;
    for (headers, expected) in [
        (
            HashMap::from([("bad header".to_string(), "secret".to_string())]),
            "invalid OTLP header name",
        ),
        (
            HashMap::from([("authorization".to_string(), "secret\nvalue".to_string())]),
            "invalid OTLP header value",
        ),
        (
            HashMap::from([
                ("Authorization".to_string(), "secret-one".to_string()),
                ("authorization".to_string(), "secret-two".to_string()),
            ]),
            "duplicate OTLP header name",
        ),
    ] {
        for signal in 0..3 {
            let exporter = OtelExporter::OtlpGrpc {
                endpoint: "http://127.0.0.1:1".to_string(),
                headers: headers.clone(),
                tls: None,
            };
            let settings = OtelSettings {
                environment: "test".to_string(),
                service_name: "test".to_string(),
                service_version: "1".to_string(),
                codex_home: ".".into(),
                exporter: if signal == 0 {
                    exporter.clone()
                } else {
                    OtelExporter::None
                },
                trace_exporter: if signal == 1 {
                    exporter.clone()
                } else {
                    OtelExporter::None
                },
                metrics_exporter: if signal == 2 {
                    exporter
                } else {
                    OtelExporter::None
                },
                runtime_metrics: false,
                span_attributes: BTreeMap::new(),
                tracestate: BTreeMap::new(),
            };
            let error = OtelProvider::from(&settings)
                .err()
                .expect("invalid configuration must fail")
                .to_string();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("secret"), "credentials must be omitted");
        }
    }
}
