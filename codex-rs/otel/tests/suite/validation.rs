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
    for name in ["bad name".to_string(), "1bad".to_string(), "-".to_string(), "a".repeat(256)] {
        assert!(matches!(
            metrics.counter(&name, 1, &[]),
            Err(MetricsError::InvalidMetricName { name: invalid }) if invalid == name
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



#[tokio::test]
async fn grpc_exporter_tls_uses_the_shared_rustls_provider() {
    use codex_otel::OtelExporter;
    use codex_otel::OtelProvider;
    use codex_otel::OtelSettings;
    use std::collections::BTreeMap;
    use std::collections::HashMap;

    let _trace_context_config_guard = crate::harness::TRACE_CONTEXT_CONFIG_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let settings = OtelSettings {
        environment: "test".to_string(),
        service_name: "test".to_string(),
        service_version: "1".to_string(),
        codex_home: ".".into(),
        exporter: OtelExporter::OtlpGrpc {
            endpoint: "https://127.0.0.1:1".to_string(),
            headers: HashMap::new(),
            tls: None,
        },
        trace_exporter: OtelExporter::None,
        metrics_exporter: OtelExporter::None,
        runtime_metrics: false,
        span_attributes: BTreeMap::new(),
        tracestate: BTreeMap::new(),
    };
    let provider = OtelProvider::from(&settings).expect("gRPC exporter builds without connecting");

    // Without a process provider, tonic falls back to ring, which cannot
    // verify the ECDSA P-521 certificates used by some enterprise proxies.
    let installed = rustls::crypto::CryptoProvider::get_default()
        .expect("gRPC TLS setup must install the shared rustls provider");
    assert!(
        installed
            .signature_verification_algorithms
            .supported_schemes()
            .contains(&rustls::SignatureScheme::ECDSA_NISTP521_SHA512)
    );
    drop(provider);
}
