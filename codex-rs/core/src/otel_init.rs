use crate::config::Config;
use codex_config::types::OtelExporterKind as Kind;
use codex_features::Feature;
use codex_login::default_client::originator;
use codex_otel::OtelExporter;
use codex_otel::OtelProvider;
use codex_otel::OtelSettings;
use std::error::Error;
use tokio_util::task::AbortOnDropHandle;

/// Build an OpenTelemetry provider from the app Config.
///
/// Returns `None` when OTEL export is disabled.
pub fn build_provider(
    config: &Config,
    service_version: &str,
    service_name_override: Option<&str>,
    default_analytics_enabled: bool,
) -> Result<Option<OtelProvider>, Box<dyn Error>> {
    OtelProvider::from(&provider_settings(
        config,
        service_version,
        service_name_override,
        default_analytics_enabled,
    ))
}

/// Build an OpenTelemetry provider without blocking async startup on configured TLS files.
///
/// A Tokio runtime is required when an exporter reads TLS files.
pub async fn build_provider_async(
    config: &Config,
    service_version: &str,
    service_name_override: Option<&str>,
    default_analytics_enabled: bool,
) -> Result<Option<OtelProvider>, Box<dyn Error>> {
    let settings = provider_settings(
        config,
        service_version,
        service_name_override,
        default_analytics_enabled,
    );
    if ![
        &settings.exporter,
        &settings.trace_exporter,
        &settings.metrics_exporter,
    ]
    .into_iter()
    .any(exporter_reads_tls_files)
    {
        return OtelProvider::from(&settings);
    }

    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let span = tracing::Span::current();
    // Tokio's blocking pool retains the current runtime handle, which exporter
    // construction uses to choose its HTTP trace processor and start gRPC.
    let task = AbortOnDropHandle::new(tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&dispatch, || {
            span.in_scope(|| OtelProvider::from(&settings).map_err(|error| error.to_string()))
        })
    }));
    match task.await {
        Ok(result) => result.map_err(Into::into),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(error.into()),
    }
}

fn exporter_reads_tls_files(exporter: &OtelExporter) -> bool {
    match exporter {
        OtelExporter::OtlpHttp { tls, .. } | OtelExporter::OtlpGrpc { tls, .. } => {
            tls.as_ref().is_some_and(|tls| {
                tls.ca_certificate.is_some()
                    || (tls.client_certificate.is_some() && tls.client_private_key.is_some())
            })
        }
        OtelExporter::None | OtelExporter::Statsig => false,
    }
}

fn provider_settings(
    config: &Config,
    service_version: &str,
    service_name_override: Option<&str>,
    default_analytics_enabled: bool,
) -> OtelSettings {
    let to_otel_exporter = |kind: &Kind| match kind {
        Kind::None => OtelExporter::None,
        Kind::Statsig => OtelExporter::Statsig,
        Kind::OtlpHttp {
            endpoint,
            headers,
            protocol,
            tls,
        } => OtelExporter::OtlpHttp {
            endpoint: endpoint.clone(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            protocol: *protocol,
            tls: tls.clone(),
        },
        Kind::OtlpGrpc {
            endpoint,
            headers,
            tls,
        } => OtelExporter::OtlpGrpc {
            endpoint: endpoint.clone(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            tls: tls.clone(),
        },
    };

    let exporter = to_otel_exporter(&config.otel.exporter);
    let trace_exporter = to_otel_exporter(&config.otel.trace_exporter);
    // Analytics consent controls the built-in exporter, not explicit OTLP collectors.
    let metrics_exporter = if matches!(config.otel.metrics_exporter, Kind::Statsig)
        && !config
            .analytics_enabled
            .unwrap_or(default_analytics_enabled)
    {
        OtelExporter::None
    } else {
        to_otel_exporter(&config.otel.metrics_exporter)
    };

    let originator = originator();
    let service_name = service_name_override.unwrap_or(originator.value.as_str());
    let runtime_metrics = config.features.enabled(Feature::RuntimeMetrics);

    OtelSettings {
        service_name: service_name.to_string(),
        service_version: service_version.to_string(),
        codex_home: config.codex_home.to_path_buf(),
        environment: config.otel.environment.to_string(),
        exporter,
        trace_exporter,
        metrics_exporter,
        runtime_metrics,
        span_attributes: config.otel.span_attributes.clone(),
        tracestate: config.otel.tracestate.clone(),
    }
}

/// Filter predicate for exporting only Codex-owned events via OTEL.
/// Keeps events that originated from codex_otel module
pub fn codex_export_filter(meta: &tracing::Metadata<'_>) -> bool {
    meta.target().starts_with("codex_otel")
}

pub fn record_process_start(otel: Option<&OtelProvider>, originator: &str) {
    let Some(metrics) = otel.and_then(OtelProvider::metrics) else {
        return;
    };
    let _ = codex_otel::record_process_start_once(metrics, originator);
}

pub fn install_sqlite_telemetry(otel: Option<&OtelProvider>, originator: &str) {
    let Some(metrics) = otel.and_then(OtelProvider::metrics) else {
        return;
    };
    let telemetry = codex_rollout::sqlite_telemetry_recorder(metrics.clone(), originator);
    let _ = codex_state::install_process_db_telemetry(telemetry);
}
