use crate::config::OtelExporter;
use crate::config::OtelHttpProtocol;
use crate::config::OtelSettings;
use crate::config::OtelTlsConfig;
use crate::config::StatsigMetricsSettings;
use crate::metrics::MetricsClient;
use crate::metrics::MetricsConfig;
use crate::targets::is_log_export_target;
use crate::targets::is_trace_safe_target;
use gethostname::gethostname;
use opentelemetry::Context;
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::trace::Span as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::LogExporter;
use opentelemetry_otlp::OTEL_EXPORTER_OTLP_LOGS_TIMEOUT;
use opentelemetry_otlp::OTEL_EXPORTER_OTLP_TRACES_TIMEOUT;
use opentelemetry_otlp::Protocol;
use opentelemetry_otlp::SpanExporter;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_otlp::WithHttpConfig;
use opentelemetry_otlp::WithTonicConfig;
use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::BatchSpanProcessor;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::trace::Span;
use opentelemetry_sdk::trace::SpanData;
use opentelemetry_sdk::trace::SpanProcessor;
use opentelemetry_sdk::trace::Tracer;
use opentelemetry_sdk::trace::TracerProviderBuilder;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor as TokioBatchSpanProcessor;
use opentelemetry_semantic_conventions as semconv;
use std::collections::BTreeMap;
use std::error::Error;
use std::time::Duration;
use tracing::debug;
use tracing_subscriber::Layer;
use tracing_subscriber::registry::LookupSpan;

const ENV_ATTRIBUTE: &str = "env";
const HOST_NAME_ATTRIBUTE: &str = "host.name";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResourceKind {
    Logs,
    Traces,
}

pub struct OtelProvider {
    pub logger: Option<SdkLoggerProvider>,
    pub tracer_provider: Option<SdkTracerProvider>,
    pub tracer: Option<Tracer>,
    pub metrics: Option<MetricsClient>,
}

impl OtelProvider {
    pub fn shutdown(&self) {
        if let Some(metrics) = &self.metrics {
            crate::metrics::uninstall_global(metrics);
        }
        if let Some(tracer_provider) = &self.tracer_provider {
            let _ = tracer_provider.shutdown();
        }
        if let Some(metrics) = &self.metrics {
            let _ = metrics.shutdown();
        }
        if let Some(logger) = &self.logger {
            let _ = logger.shutdown();
        }
    }

    pub fn from(settings: &OtelSettings) -> Result<Option<Self>, Box<dyn Error>> {
        if matches!(settings.exporter, OtelExporter::Statsig)
            || matches!(settings.trace_exporter, OtelExporter::Statsig)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Statsig is supported only for metrics",
            )
            .into());
        }
        let log_enabled = !matches!(settings.exporter, OtelExporter::None);
        let trace_enabled = !matches!(settings.trace_exporter, OtelExporter::None);
        let metric_exporter = crate::config::resolve_exporter(&settings.metrics_exporter);
        let metrics_enabled = !matches!(metric_exporter, OtelExporter::None);

        if !log_enabled && !trace_enabled && !metrics_enabled {
            // Tracestate propagation is process-global; clear it when these
            // settings do not install an active provider.
            crate::trace_context::set_tracestate_entries(BTreeMap::new())?;
            debug!("No OTEL exporter enabled in settings.");
            return Ok(None);
        }

        // Provider setup installs process-global OTEL state that cannot be
        // rolled back. Validate trace metadata before any setup path can
        // mutate those globals, and keep span attribute checks aligned with
        // config loading when traces are exported.
        if trace_enabled {
            crate::config::validate_span_attributes(&settings.span_attributes)?;
        }
        crate::trace_context::validate_tracestate_entries(&settings.tracestate)?;

        let metrics = if matches!(metric_exporter, OtelExporter::None) {
            None
        } else {
            let mut config = MetricsConfig::otlp(
                settings.environment.clone(),
                settings.service_name.clone(),
                settings.service_version.clone(),
                metric_exporter,
            );
            if settings.runtime_metrics {
                config = config.with_runtime_reader();
            }
            Some(MetricsClient::new(config)?)
        };

        let logger = log_enabled
            .then(|| {
                let resource = make_resource(settings, ResourceKind::Logs);
                build_logger(&resource, &settings.exporter)
            })
            .transpose()?;

        let tracer_provider = trace_enabled
            .then(|| {
                let resource = make_resource(settings, ResourceKind::Traces);
                build_tracer_provider(
                    &resource,
                    &settings.trace_exporter,
                    settings.span_attributes.clone(),
                )
            })
            .transpose()?;

        let tracer = tracer_provider
            .as_ref()
            .map(|provider| provider.tracer(settings.service_name.clone()));

        crate::trace_context::set_tracestate_entries(settings.tracestate.clone())?;
        if let Some(provider) = tracer_provider.clone() {
            global::set_tracer_provider(provider);
            global::set_text_map_propagator(TraceContextPropagator::new());
        }
        if let Some(metrics) = metrics.as_ref() {
            let statsig = matches!(settings.metrics_exporter, OtelExporter::Statsig).then(|| {
                StatsigMetricsSettings {
                    environment: settings.environment.clone(),
                }
            });
            crate::metrics::install_global(metrics.clone(), statsig);
        }
        Ok(Some(Self {
            logger,
            tracer_provider,
            tracer,
            metrics,
        }))
    }

    pub fn logger_layer<S>(&self) -> Option<impl Layer<S> + Send + Sync>
    where
        S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
    {
        self.logger.as_ref().map(|logger| {
            OpenTelemetryTracingBridge::new(logger).with_filter(
                tracing_subscriber::filter::filter_fn(OtelProvider::log_export_filter),
            )
        })
    }

    pub fn tracing_layer<S>(&self) -> Option<impl Layer<S> + Send + Sync>
    where
        S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
    {
        self.tracer.as_ref().map(|tracer| {
            tracing_opentelemetry::layer()
                .with_tracer(tracer.clone())
                .with_filter(tracing_subscriber::filter::filter_fn(
                    OtelProvider::trace_export_filter,
                ))
        })
    }

    pub fn codex_export_filter(meta: &tracing::Metadata<'_>) -> bool {
        Self::log_export_filter(meta)
    }

    pub fn log_export_filter(meta: &tracing::Metadata<'_>) -> bool {
        is_log_export_target(meta.target())
    }

    pub fn trace_export_filter(meta: &tracing::Metadata<'_>) -> bool {
        meta.is_span() || is_trace_safe_target(meta.target())
    }

    pub fn metrics(&self) -> Option<&MetricsClient> {
        self.metrics.as_ref()
    }
}

impl Drop for OtelProvider {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn make_resource(settings: &OtelSettings, kind: ResourceKind) -> Resource {
    let host_name = (kind == ResourceKind::Logs).then(detected_host_name).flatten();
    Resource::builder()
        .with_service_name(settings.service_name.clone())
        .with_attributes(resource_attributes(
            settings,
            host_name.as_deref(),
            kind,
        ))
        .build()
}

fn resource_attributes(
    settings: &OtelSettings,
    host_name: Option<&str>,
    kind: ResourceKind,
) -> Vec<KeyValue> {
    let mut attributes = vec![
        KeyValue::new(
            semconv::attribute::SERVICE_VERSION,
            settings.service_version.clone(),
        ),
        KeyValue::new(ENV_ATTRIBUTE, settings.environment.clone()),
    ];
    if kind == ResourceKind::Logs
        && let Some(host_name) = host_name.and_then(normalize_host_name)
    {
        attributes.push(KeyValue::new(HOST_NAME_ATTRIBUTE, host_name));
    }
    attributes
}

fn detected_host_name() -> Option<String> {
    let host_name = gethostname();
    normalize_host_name(host_name.to_string_lossy().as_ref())
}

fn normalize_host_name(host_name: &str) -> Option<String> {
    let host_name = host_name.trim();
    (!host_name.is_empty()).then(|| host_name.to_owned())
}

fn tracer_provider_builder<P: SpanProcessor + 'static>(
    resource: &Resource,
    span_attributes: BTreeMap<String, String>,
    processor: P,
) -> TracerProviderBuilder {
    let builder = SdkTracerProvider::builder().with_resource(resource.clone());
    if span_attributes.is_empty() {
        builder.with_span_processor(processor)
    } else {
        builder.with_span_processor(SpanAttributesProcessor {
            attributes: span_attributes,
            inner: processor,
        })
    }
}

/// Applies configured attributes when spans start.
///
/// Resource attributes describe the provider process. These attributes are
/// per-span metadata, so they need to be attached before each span is exported.
/// Wrap the exporter processor so the SDK can move completed span data into one
/// processor instead of cloning it for a separate attribute-only processor.
#[derive(Debug)]
struct SpanAttributesProcessor<P> {
    inner: P,
    attributes: BTreeMap<String, String>,
}

impl<P: SpanProcessor> SpanProcessor for SpanAttributesProcessor<P> {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        for (key, value) in self.attributes.iter() {
            span.set_attribute(KeyValue::new(key.clone(), value.clone()));
        }
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, span: SpanData) {
        self.inner.on_end(span);
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

fn build_logger(
    resource: &Resource,
    exporter: &OtelExporter,
) -> Result<SdkLoggerProvider, Box<dyn Error>> {
    let mut builder = SdkLoggerProvider::builder().with_resource(resource.clone());

    match crate::config::resolve_exporter(exporter) {
        OtelExporter::None => return Ok(builder.build()),
        OtelExporter::Statsig => unreachable!("statsig exporter should be resolved"),
        OtelExporter::OtlpGrpc {
            endpoint,
            headers,
            tls,
        } => {
            debug!("Using OTLP Grpc exporter: {endpoint}");

            let header_map = crate::otlp::build_header_map(&headers)?;
            let tls_config = crate::otlp::grpc_tls_config(&endpoint, tls.as_ref())?;

            let exporter = LogExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint)
                .with_metadata(MetadataMap::from_headers(header_map))
                .with_tls_config(tls_config)
                .build()?;

            builder = builder.with_batch_exporter(exporter);
        }
        OtelExporter::OtlpHttp {
            endpoint,
            headers,
            protocol,
            tls,
        } => {
            debug!("Using OTLP Http exporter: {endpoint}");
            crate::otlp::build_header_map(&headers)?;

            let protocol = match protocol {
                OtelHttpProtocol::Binary => Protocol::HttpBinary,
                OtelHttpProtocol::Json => Protocol::HttpJson,
            };

            let mut exporter_builder = LogExporter::builder()
                .with_http()
                .with_endpoint(endpoint)
                .with_protocol(protocol)
                .with_headers(headers);

            let client = crate::otlp::build_http_client(
                tls.as_ref().unwrap_or(&OtelTlsConfig::default()),
                OTEL_EXPORTER_OTLP_LOGS_TIMEOUT,
            )?;
            exporter_builder = exporter_builder.with_http_client(client);

            let exporter = exporter_builder.build()?;

            builder = builder.with_batch_exporter(exporter);
        }
    }

    Ok(builder.build())
}

fn build_tracer_provider(
    resource: &Resource,
    exporter: &OtelExporter,
    span_attributes: BTreeMap<String, String>,
) -> Result<SdkTracerProvider, Box<dyn Error>> {
    let span_exporter = match crate::config::resolve_exporter(exporter) {
        OtelExporter::None => {
            return Ok(SdkTracerProvider::builder()
                .with_resource(resource.clone())
                .build());
        }
        OtelExporter::Statsig => unreachable!("statsig exporter should be resolved"),
        OtelExporter::OtlpGrpc {
            endpoint,
            headers,
            tls,
        } => {
            debug!("Using OTLP Grpc exporter for traces: {endpoint}");

            let header_map = crate::otlp::build_header_map(&headers)?;
            let tls_config = crate::otlp::grpc_tls_config(&endpoint, tls.as_ref())?;

            SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint)
                .with_metadata(MetadataMap::from_headers(header_map))
                .with_tls_config(tls_config)
                .build()?
        }
        OtelExporter::OtlpHttp {
            endpoint,
            headers,
            protocol,
            tls,
        } => {
            debug!("Using OTLP Http exporter for traces: {endpoint}");
            crate::otlp::build_header_map(&headers)?;

            if crate::otlp::current_tokio_runtime_is_multi_thread() {
                let protocol = match protocol {
                    OtelHttpProtocol::Binary => Protocol::HttpBinary,
                    OtelHttpProtocol::Json => Protocol::HttpJson,
                };

                let mut exporter_builder = SpanExporter::builder()
                    .with_http()
                    .with_endpoint(endpoint)
                    .with_protocol(protocol)
                    .with_headers(headers);

                let client = crate::otlp::build_async_http_client(
                    tls.as_ref(),
                    OTEL_EXPORTER_OTLP_TRACES_TIMEOUT,
                )?;
                exporter_builder = exporter_builder.with_http_client(client);

                let processor =
                    TokioBatchSpanProcessor::builder(exporter_builder.build()?, runtime::Tokio)
                        .build();

                return Ok(tracer_provider_builder(resource, span_attributes, processor).build());
            }

            let protocol = match protocol {
                OtelHttpProtocol::Binary => Protocol::HttpBinary,
                OtelHttpProtocol::Json => Protocol::HttpJson,
            };

            let mut exporter_builder = SpanExporter::builder()
                .with_http()
                .with_endpoint(endpoint)
                .with_protocol(protocol)
                .with_headers(headers);

            let client = crate::otlp::build_http_client(
                tls.as_ref().unwrap_or(&OtelTlsConfig::default()),
                OTEL_EXPORTER_OTLP_TRACES_TIMEOUT,
            )?;
            exporter_builder = exporter_builder.with_http_client(client);

            exporter_builder.build()?
        }
    };

    let processor = BatchSpanProcessor::builder(span_exporter).build();

    Ok(tracer_provider_builder(resource, span_attributes, processor).build())
}

