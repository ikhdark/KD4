use codex_api::AgentIdentityTelemetry;
use codex_otel::AuthEnvTelemetryMetadata;
use codex_otel::OtelProvider;
use codex_otel::SessionTelemetry;
use codex_otel::TelemetryAuthMode;
use opentelemetry::KeyValue;
use opentelemetry::logs::AnyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::logs::InMemoryLogExporter;
use opentelemetry_sdk::logs::SdkLogRecord;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::SdkTracerProvider;
use pretty_assertions::assert_eq;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;

use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;

fn log_attributes(record: &SdkLogRecord) -> BTreeMap<String, String> {
    record
        .attributes_iter()
        .map(|(key, value)| (key.as_str().to_string(), any_value_to_string(value)))
        .collect()
}

fn span_event_attributes(event: &opentelemetry::trace::Event) -> BTreeMap<String, String> {
    event
        .attributes
        .iter()
        .map(|KeyValue { key, value, .. }| (key.as_str().to_string(), value.to_string()))
        .collect()
}

fn any_value_to_string(value: &AnyValue) -> String {
    match value {
        AnyValue::Int(value) => value.to_string(),
        AnyValue::Double(value) => value.to_string(),
        AnyValue::String(value) => value.as_str().to_string(),
        AnyValue::Boolean(value) => value.to_string(),
        AnyValue::Bytes(value) => String::from_utf8_lossy(value).into_owned(),
        AnyValue::ListAny(value) => format!("{value:?}"),
        AnyValue::Map(value) => format!("{value:?}"),
        _ => format!("{value:?}"),
    }
}

fn find_log_by_event_name<'a>(
    logs: &'a [opentelemetry_sdk::logs::in_memory_exporter::LogDataWithResource],
    event_name: &str,
) -> &'a opentelemetry_sdk::logs::in_memory_exporter::LogDataWithResource {
    logs.iter()
        .find(|log| {
            log_attributes(&log.record)
                .get("event.name")
                .is_some_and(|value| value == event_name)
        })
        .expect("log event should exist")
}

fn find_span_event_by_name_attr<'a>(
    events: &'a [opentelemetry::trace::Event],
    event_name: &str,
) -> &'a opentelemetry::trace::Event {
    events
        .iter()
        .find(|event| {
            span_event_attributes(event)
                .get("event.name")
                .is_some_and(|value| value == event_name)
        })
        .expect("span event should exist")
}

fn auth_env_metadata() -> AuthEnvTelemetryMetadata {
    AuthEnvTelemetryMetadata {
        openai_api_key_env_present: true,
        codex_api_key_env_present: false,
        codex_api_key_env_enabled: true,
        provider_env_key_name: Some("configured".to_string()),
        provider_env_key_present: Some(true),
        refresh_token_url_override_present: true,
    }
}

#[test]
fn export_routing_requires_target_namespace_boundaries() {
    let log_exporter = InMemoryLogExporter::default();
    let logger_provider = SdkLoggerProvider::builder()
        .with_simple_exporter(log_exporter.clone())
        .build();
    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                &logger_provider,
            )
            .with_filter(filter_fn(OtelProvider::log_export_filter)),
        )
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer_provider.tracer("target-boundaries"))
                .with_filter(filter_fn(OtelProvider::trace_export_filter)),
        );
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("root");
        let _guard = span.enter();
        tracing::event!(target: "codex_otel", tracing::Level::INFO, event.name = "root", "test OTEL routing");
        tracing::event!(target: "codex_otel::module", tracing::Level::INFO, event.name = "module", "test OTEL routing");
        tracing::event!(target: "codex_otel.trace_safe_extra", tracing::Level::INFO, event.name = "near_safe", "test OTEL routing");
        tracing::event!(target: "codex_otel_unrelated", tracing::Level::INFO, event.name = "unrelated", "test OTEL routing");
        tracing::event!(target: "codex_otel.trace_safe", tracing::Level::INFO, event.name = "safe", "test OTEL routing");
        tracing::event!(target: "codex_otel.trace_safe.child", tracing::Level::INFO, event.name = "safe_child", "test OTEL routing");
    });
    logger_provider.force_flush().expect("flush logs");
    tracer_provider.force_flush().expect("flush spans");
    let logs = log_exporter.get_emitted_logs().expect("logs");
    let names: Vec<_> = logs
        .iter()
        .map(|log| log_attributes(&log.record)["event.name"].clone())
        .collect();
    assert_eq!(names, ["root", "module", "near_safe"]);
    let spans = span_exporter.get_finished_spans().expect("spans");
    assert_eq!(spans.len(), 1);
    let names: Vec<_> = spans[0]
        .events
        .events
        .iter()
        .map(|event| span_event_attributes(event)["event.name"].clone())
        .collect();
    assert_eq!(names, ["safe", "safe_child"]);
}

#[test]
fn otel_export_routing_policy_routes_user_prompt_log_and_trace_events() {
    let log_exporter = InMemoryLogExporter::default();
    let logger_provider = SdkLoggerProvider::builder()
        .with_simple_exporter(log_exporter.clone())
        .build();
    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter.clone())
        .build();
    let tracer = tracer_provider.tracer("sink-split-test");

    let subscriber = tracing_subscriber::registry()
        .with(
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                &logger_provider,
            )
            .with_filter(filter_fn(OtelProvider::log_export_filter)),
        )
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(filter_fn(OtelProvider::trace_export_filter)),
        );

    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        let manager = SessionTelemetry::new(
            ThreadId::new(),
            "gpt-5.1",
            "gpt-5.1",
            Some("account-id".to_string()),
            Some("engineer@example.com".to_string()),
            Some(TelemetryAuthMode::ApiKey),
            "codex_exec".to_string(),
            /*log_user_prompts*/ true,
            "tty".to_string(),
            SessionSource::Cli,
        );
        let root_span = tracing::info_span!("root");
        let _root_guard = root_span.enter();
        manager.user_prompt(&[
            UserInput::Text {
                text: "super secret prompt".to_string(),
                text_elements: Vec::new(),
            },
            UserInput::Image {
                image_url: "https://example.com/image.png".to_string(),
                detail: None,
            },
            UserInput::LocalImage {
                path: PathBuf::from("/tmp/secret.png"),
                detail: None,
            },
        ]);
    });

    logger_provider.force_flush().expect("flush logs");
    tracer_provider.force_flush().expect("flush traces");

    let logs = log_exporter.get_emitted_logs().expect("log export");
    assert!(
        logs.iter()
            .all(|log| { log.record.target().map(Cow::as_ref) == Some("codex_otel.log_only") })
    );

    let prompt_log = find_log_by_event_name(&logs, "codex.user_prompt");
    let prompt_log_attrs = log_attributes(&prompt_log.record);
    assert_eq!(
        prompt_log_attrs.get("prompt").map(String::as_str),
        Some("super secret prompt")
    );
    assert_eq!(
        prompt_log_attrs.get("user.email").map(String::as_str),
        Some("engineer@example.com")
    );

    let spans = span_exporter.get_finished_spans().expect("span export");
    assert_eq!(spans.len(), 1);
    let span_events = &spans[0].events.events;
    assert_eq!(span_events.len(), 1);

    let prompt_trace_event = find_span_event_by_name_attr(span_events, "codex.user_prompt");
    let prompt_trace_attrs = span_event_attributes(prompt_trace_event);
    assert_eq!(
        prompt_trace_attrs.get("prompt_length").map(String::as_str),
        Some("19")
    );
    assert_eq!(
        prompt_trace_attrs
            .get("text_input_count")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        prompt_trace_attrs
            .get("image_input_count")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        prompt_trace_attrs
            .get("local_image_input_count")
            .map(String::as_str),
        Some("1")
    );
    assert!(!prompt_trace_attrs.contains_key("prompt"));
    assert!(!prompt_trace_attrs.contains_key("user.email"));
    assert!(!prompt_trace_attrs.contains_key("user.account_id"));
}

#[test]
fn otel_export_routing_policy_routes_tool_result_log_and_trace_events() {
    let log_exporter = InMemoryLogExporter::default();
    let logger_provider = SdkLoggerProvider::builder()
        .with_simple_exporter(log_exporter.clone())
        .build();
    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter.clone())
        .build();
    let tracer = tracer_provider.tracer("sink-split-test");

    let subscriber = tracing_subscriber::registry()
        .with(
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                &logger_provider,
            )
            .with_filter(filter_fn(OtelProvider::log_export_filter)),
        )
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(filter_fn(OtelProvider::trace_export_filter)),
        );

    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        let manager = SessionTelemetry::new(
            ThreadId::new(),
            "gpt-5.1",
            "gpt-5.1",
            Some("account-id".to_string()),
            Some("engineer@example.com".to_string()),
            Some(TelemetryAuthMode::ApiKey),
            "codex_exec".to_string(),
            /*log_user_prompts*/ true,
            "tty".to_string(),
            SessionSource::Cli,
        );
        let root_span = tracing::info_span!("root");
        let _root_guard = root_span.enter();
        manager.tool_result_with_tags(
            "shell",
            "call-1",
            "secret arguments",
            std::time::Duration::from_millis(42),
            /*success*/ true,
            "secret output\nsecond line",
            &[],
            &[
                ("mcp_server", "internal-mcp"),
                ("mcp_server_origin", "stdio"),
            ],
        );
    });

    logger_provider.force_flush().expect("flush logs");
    tracer_provider.force_flush().expect("flush traces");

    let logs = log_exporter.get_emitted_logs().expect("log export");
    assert!(
        logs.iter()
            .all(|log| { log.record.target().map(Cow::as_ref) == Some("codex_otel.log_only") })
    );

    let tool_log = find_log_by_event_name(&logs, "codex.tool_result");
    let tool_log_attrs = log_attributes(&tool_log.record);
    assert_eq!(
        tool_log_attrs.get("arguments_length").map(String::as_str),
        Some("16")
    );
    assert_eq!(
        tool_log_attrs.get("output_length").map(String::as_str),
        Some("25")
    );
    assert_eq!(
        tool_log_attrs.get("output_line_count").map(String::as_str),
        Some("2")
    );
    assert!(!tool_log_attrs.contains_key("arguments"));
    assert!(!tool_log_attrs.contains_key("output"));
    assert_eq!(
        tool_log_attrs.get("mcp_server").map(String::as_str),
        Some("internal-mcp")
    );
    assert_eq!(
        tool_log_attrs.get("mcp_server_origin").map(String::as_str),
        Some("stdio")
    );

    let spans = span_exporter.get_finished_spans().expect("span export");
    assert_eq!(spans.len(), 1);
    let span_events = &spans[0].events.events;
    assert_eq!(span_events.len(), 1);

    let tool_trace_event = find_span_event_by_name_attr(span_events, "codex.tool_result");
    let tool_trace_attrs = span_event_attributes(tool_trace_event);
    assert_eq!(
        tool_trace_attrs.get("arguments_length").map(String::as_str),
        Some("16")
    );
    assert_eq!(
        tool_trace_attrs.get("output_length").map(String::as_str),
        Some("25")
    );
    assert_eq!(
        tool_trace_attrs
            .get("output_line_count")
            .map(String::as_str),
        Some("2")
    );
    assert!(!tool_trace_attrs.contains_key("arguments"));
    assert!(!tool_trace_attrs.contains_key("output"));
    assert!(!tool_trace_attrs.contains_key("mcp_server"));
    assert!(!tool_trace_attrs.contains_key("mcp_server_origin"));
}

#[test]
fn logging_contract_failed_tool_event_omits_error_output() {
    let log_exporter = InMemoryLogExporter::default();
    let logger_provider = SdkLoggerProvider::builder()
        .with_simple_exporter(log_exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry().with(
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&logger_provider)
            .with_filter(filter_fn(OtelProvider::log_export_filter)),
    );

    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        let manager = SessionTelemetry::new(
            ThreadId::new(),
            "gpt-5.1",
            "gpt-5.1",
            None,
            None,
            Some(TelemetryAuthMode::ApiKey),
            "codex_exec".to_string(),
            true,
            "tty".to_string(),
            SessionSource::Cli,
        );
        manager.log_tool_failed("shell", "failure secret\nsecond line");
    });

    logger_provider.force_flush().expect("flush logs");
    let logs = log_exporter.get_emitted_logs().expect("log export");
    let tool_log = find_log_by_event_name(&logs, "codex.tool_result");
    let attributes = log_attributes(&tool_log.record);
    assert_eq!(
        attributes.get("output_length").map(String::as_str),
        Some("26")
    );
    assert_eq!(
        attributes.get("output_line_count").map(String::as_str),
        Some("2")
    );
    assert!(!attributes.contains_key("output"));
    assert!(!attributes.contains_key("error.message"));
}

#[test]
fn otel_export_routing_policy_routes_auth_recovery_log_and_trace_events() {
    let log_exporter = InMemoryLogExporter::default();
    let logger_provider = SdkLoggerProvider::builder()
        .with_simple_exporter(log_exporter.clone())
        .build();
    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter.clone())
        .build();
    let tracer = tracer_provider.tracer("sink-split-test");

    let subscriber = tracing_subscriber::registry()
        .with(
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                &logger_provider,
            )
            .with_filter(filter_fn(OtelProvider::log_export_filter)),
        )
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(filter_fn(OtelProvider::trace_export_filter)),
        );

    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        let manager = SessionTelemetry::new(
            ThreadId::new(),
            "gpt-5.1",
            "gpt-5.1",
            Some("account-id".to_string()),
            Some("engineer@example.com".to_string()),
            Some(TelemetryAuthMode::Chatgpt),
            "codex_exec".to_string(),
            /*log_user_prompts*/ true,
            "tty".to_string(),
            SessionSource::Cli,
        );
        let root_span = tracing::info_span!("root");
        let _root_guard = root_span.enter();
        manager.record_auth_recovery(
            "managed",
            "reload",
            "recovery_succeeded",
            Some("req-401"),
            Some("ray-401"),
            Some("missing_authorization_header"),
            Some("token_expired"),
            /*recovery_reason*/ None,
            Some(true),
        );
    });

    logger_provider.force_flush().expect("flush logs");
    tracer_provider.force_flush().expect("flush traces");

    let logs = log_exporter.get_emitted_logs().expect("log export");
    let recovery_log = find_log_by_event_name(&logs, "codex.auth_recovery");
    let recovery_log_attrs = log_attributes(&recovery_log.record);
    assert_eq!(
        recovery_log_attrs.get("auth.mode").map(String::as_str),
        Some("managed")
    );
    assert_eq!(
        recovery_log_attrs.get("auth.step").map(String::as_str),
        Some("reload")
    );
    assert_eq!(
        recovery_log_attrs.get("auth.outcome").map(String::as_str),
        Some("recovery_succeeded")
    );
    assert_eq!(
        recovery_log_attrs
            .get("auth.request_id")
            .map(String::as_str),
        Some("req-401")
    );
    assert_eq!(
        recovery_log_attrs.get("auth.cf_ray").map(String::as_str),
        Some("ray-401")
    );
    assert_eq!(
        recovery_log_attrs.get("auth.error").map(String::as_str),
        Some("missing_authorization_header")
    );
    assert_eq!(
        recovery_log_attrs
            .get("auth.error_code")
            .map(String::as_str),
        Some("token_expired")
    );
    assert_eq!(
        recovery_log_attrs
            .get("auth.state_changed")
            .map(String::as_str),
        Some("true")
    );

    let spans = span_exporter.get_finished_spans().expect("span export");
    assert_eq!(spans.len(), 1);
    let span_events = &spans[0].events.events;
    assert_eq!(span_events.len(), 1);

    let recovery_trace_event = find_span_event_by_name_attr(span_events, "codex.auth_recovery");
    let recovery_trace_attrs = span_event_attributes(recovery_trace_event);
    assert_eq!(
        recovery_trace_attrs
            .get("auth.error_present")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        recovery_trace_attrs.get("auth.mode").map(String::as_str),
        Some("managed")
    );
    assert_eq!(
        recovery_trace_attrs.get("auth.step").map(String::as_str),
        Some("reload")
    );
    assert_eq!(
        recovery_trace_attrs.get("auth.outcome").map(String::as_str),
        Some("recovery_succeeded")
    );
    assert_eq!(
        recovery_trace_attrs
            .get("auth.request_id")
            .map(String::as_str),
        Some("req-401")
    );
    assert_eq!(
        recovery_trace_attrs.get("auth.cf_ray").map(String::as_str),
        Some("ray-401")
    );
    assert_eq!(
        recovery_trace_attrs.get("auth.error").map(String::as_str),
        None
    );
    assert_eq!(
        recovery_trace_attrs
            .get("auth.error_code")
            .map(String::as_str),
        None
    );
    assert_eq!(
        recovery_trace_attrs
            .get("auth.state_changed")
            .map(String::as_str),
        Some("true")
    );
}

#[test]
fn otel_export_routing_policy_routes_api_request_auth_observability() {
    let log_exporter = InMemoryLogExporter::default();
    let logger_provider = SdkLoggerProvider::builder()
        .with_simple_exporter(log_exporter.clone())
        .build();
    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter.clone())
        .build();
    let tracer = tracer_provider.tracer("sink-split-test");

    let subscriber = tracing_subscriber::registry()
        .with(
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                &logger_provider,
            )
            .with_filter(filter_fn(OtelProvider::log_export_filter)),
        )
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(filter_fn(OtelProvider::trace_export_filter)),
        );

    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        let manager = SessionTelemetry::new(
            ThreadId::new(),
            "gpt-5.1",
            "gpt-5.1",
            Some("account-id".to_string()),
            Some("engineer@example.com".to_string()),
            Some(TelemetryAuthMode::Chatgpt),
            "codex_exec".to_string(),
            /*log_user_prompts*/ true,
            "tty".to_string(),
            SessionSource::Cli,
        )
        .with_auth_env(auth_env_metadata());
        let root_span = tracing::info_span!("root");
        let _root_guard = root_span.enter();
        manager.conversation_starts(
            "openai",
            /*reasoning_effort*/ None,
            ReasoningSummary::Auto,
            /*context_window*/ None,
            /*auto_compact_token_limit*/ None,
            AskForApproval::Never,
            SandboxPolicy::DangerFullAccess,
            Vec::new(),
        );
        let agent_identity_telemetry = AgentIdentityTelemetry {
            agent_id: "agent-runtime-otel".to_string(),
            task_id: "task-run-otel".to_string(),
        };
        manager.record_api_request(
            /*attempt*/ 1,
            Some(401),
            Some("http 401"),
            std::time::Duration::from_millis(42),
            /*auth_header_attached*/ true,
            Some("authorization"),
            /*retry_after_unauthorized*/ true,
            Some("managed"),
            Some("refresh_token"),
            "/responses",
            Some("req-401"),
            Some("ray-401"),
            Some("missing_authorization_header"),
            Some("token_expired"),
            Some(&agent_identity_telemetry),
        );
    });

    logger_provider.force_flush().expect("flush logs");
    tracer_provider.force_flush().expect("flush traces");

    let logs = log_exporter.get_emitted_logs().expect("log export");
    let conversation_log = find_log_by_event_name(&logs, "codex.conversation_starts");
    let conversation_log_attrs = log_attributes(&conversation_log.record);
    assert_eq!(
        conversation_log_attrs
            .get("auth.env_openai_api_key_present")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        conversation_log_attrs
            .get("auth.env_provider_key_name")
            .map(String::as_str),
        Some("configured")
    );
    let request_log = find_log_by_event_name(&logs, "codex.api_request");
    let request_log_attrs = log_attributes(&request_log.record);
    assert_eq!(
        request_log_attrs
            .get("auth.header_attached")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        request_log_attrs
            .get("auth.header_name")
            .map(String::as_str),
        Some("authorization")
    );
    assert_eq!(
        request_log_attrs
            .get("auth.retry_after_unauthorized")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        request_log_attrs
            .get("auth.recovery_mode")
            .map(String::as_str),
        Some("managed")
    );
    assert_eq!(
        request_log_attrs
            .get("auth.recovery_phase")
            .map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        request_log_attrs.get("endpoint").map(String::as_str),
        Some("/responses")
    );
    assert_eq!(
        request_log_attrs.get("auth.error").map(String::as_str),
        Some("missing_authorization_header")
    );
    assert_eq!(
        request_log_attrs
            .get("auth.env_codex_api_key_enabled")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        request_log_attrs
            .get("auth.env_refresh_token_url_override_present")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        request_log_attrs.get("auth.agent_id").map(String::as_str),
        Some("agent-runtime-otel")
    );
    assert_eq!(
        request_log_attrs.get("auth.task_id").map(String::as_str),
        Some("task-run-otel")
    );

    let spans = span_exporter.get_finished_spans().expect("span export");
    let conversation_trace_event =
        find_span_event_by_name_attr(&spans[0].events.events, "codex.conversation_starts");
    let conversation_trace_attrs = span_event_attributes(conversation_trace_event);
    assert_eq!(
        conversation_trace_attrs
            .get("auth.env_provider_key_present")
            .map(String::as_str),
        Some("true")
    );
    let request_trace_event =
        find_span_event_by_name_attr(&spans[0].events.events, "codex.api_request");
    let request_trace_attrs = span_event_attributes(request_trace_event);
    assert_eq!(
        request_trace_attrs.get("error.present").map(String::as_str),
        Some("true")
    );
    for key in ["error.message", "endpoint", "auth.error", "auth.error_code"] {
        assert!(
            !request_trace_attrs.contains_key(key),
            "unexpected trace diagnostic: {key}"
        );
    }
    assert_eq!(
        request_trace_attrs
            .get("auth.header_attached")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        request_trace_attrs
            .get("auth.header_name")
            .map(String::as_str),
        Some("authorization")
    );
    assert_eq!(
        request_trace_attrs
            .get("auth.retry_after_unauthorized")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        request_trace_attrs.get("endpoint").map(String::as_str),
        None
    );
    assert_eq!(
        request_trace_attrs
            .get("auth.env_openai_api_key_present")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        request_trace_attrs.get("auth.agent_id").map(String::as_str),
        Some("agent-runtime-otel")
    );
    assert_eq!(
        request_trace_attrs.get("auth.task_id").map(String::as_str),
        Some("task-run-otel")
    );
}

#[test]
fn otel_export_routing_policy_routes_websocket_connect_auth_observability() {
    let log_exporter = InMemoryLogExporter::default();
    let logger_provider = SdkLoggerProvider::builder()
        .with_simple_exporter(log_exporter.clone())
        .build();
    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter.clone())
        .build();
    let tracer = tracer_provider.tracer("sink-split-test");

    let subscriber = tracing_subscriber::registry()
        .with(
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                &logger_provider,
            )
            .with_filter(filter_fn(OtelProvider::log_export_filter)),
        )
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(filter_fn(OtelProvider::trace_export_filter)),
        );

    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        let manager = SessionTelemetry::new(
            ThreadId::new(),
            "gpt-5.1",
            "gpt-5.1",
            Some("account-id".to_string()),
            Some("engineer@example.com".to_string()),
            Some(TelemetryAuthMode::Chatgpt),
            "codex_exec".to_string(),
            /*log_user_prompts*/ true,
            "tty".to_string(),
            SessionSource::Cli,
        )
        .with_auth_env(auth_env_metadata());
        let root_span = tracing::info_span!("root");
        let _root_guard = root_span.enter();
        let agent_identity_telemetry = AgentIdentityTelemetry {
            agent_id: "agent-runtime-ws".to_string(),
            task_id: "task-run-ws".to_string(),
        };
        manager.record_websocket_connect(
            std::time::Duration::from_millis(17),
            Some(401),
            Some("http 401"),
            /*auth_header_attached*/ true,
            Some("authorization"),
            /*retry_after_unauthorized*/ true,
            Some("managed"),
            Some("reload"),
            "/responses",
            /*connection_reused*/ false,
            Some("req-ws-401"),
            Some("ray-ws-401"),
            Some("missing_authorization_header"),
            Some("token_expired"),
            Some(&agent_identity_telemetry),
        );
    });

    logger_provider.force_flush().expect("flush logs");
    tracer_provider.force_flush().expect("flush traces");

    let logs = log_exporter.get_emitted_logs().expect("log export");
    let connect_log = find_log_by_event_name(&logs, "codex.websocket_connect");
    let connect_log_attrs = log_attributes(&connect_log.record);
    assert_eq!(
        connect_log_attrs
            .get("auth.header_attached")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        connect_log_attrs
            .get("auth.header_name")
            .map(String::as_str),
        Some("authorization")
    );
    assert_eq!(
        connect_log_attrs.get("auth.error").map(String::as_str),
        Some("missing_authorization_header")
    );
    assert_eq!(
        connect_log_attrs.get("endpoint").map(String::as_str),
        Some("/responses")
    );
    assert_eq!(
        connect_log_attrs
            .get("auth.connection_reused")
            .map(String::as_str),
        Some("false")
    );
    assert_eq!(
        connect_log_attrs
            .get("auth.env_provider_key_name")
            .map(String::as_str),
        Some("configured")
    );
    assert_eq!(
        connect_log_attrs.get("auth.agent_id").map(String::as_str),
        Some("agent-runtime-ws")
    );
    assert_eq!(
        connect_log_attrs.get("auth.task_id").map(String::as_str),
        Some("task-run-ws")
    );

    let spans = span_exporter.get_finished_spans().expect("span export");
    let connect_trace_event =
        find_span_event_by_name_attr(&spans[0].events.events, "codex.websocket_connect");
    let connect_trace_attrs = span_event_attributes(connect_trace_event);
    assert_eq!(
        connect_trace_attrs.get("error.present").map(String::as_str),
        Some("true")
    );
    for key in ["error.message", "endpoint", "auth.error", "auth.error_code"] {
        assert!(
            !connect_trace_attrs.contains_key(key),
            "unexpected trace diagnostic: {key}"
        );
    }
    assert_eq!(
        connect_trace_attrs
            .get("auth.recovery_phase")
            .map(String::as_str),
        Some("reload")
    );
    assert_eq!(
        connect_trace_attrs
            .get("auth.env_refresh_token_url_override_present")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        connect_trace_attrs.get("auth.agent_id").map(String::as_str),
        Some("agent-runtime-ws")
    );
    assert_eq!(
        connect_trace_attrs.get("auth.task_id").map(String::as_str),
        Some("task-run-ws")
    );
}

#[test]
fn otel_export_routing_policy_routes_websocket_request_transport_observability() {
    let log_exporter = InMemoryLogExporter::default();
    let logger_provider = SdkLoggerProvider::builder()
        .with_simple_exporter(log_exporter.clone())
        .build();
    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter.clone())
        .build();
    let tracer = tracer_provider.tracer("sink-split-test");

    let subscriber = tracing_subscriber::registry()
        .with(
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                &logger_provider,
            )
            .with_filter(filter_fn(OtelProvider::log_export_filter)),
        )
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(filter_fn(OtelProvider::trace_export_filter)),
        );

    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        let manager = SessionTelemetry::new(
            ThreadId::new(),
            "gpt-5.1",
            "gpt-5.1",
            Some("account-id".to_string()),
            Some("engineer@example.com".to_string()),
            Some(TelemetryAuthMode::Chatgpt),
            "codex_exec".to_string(),
            /*log_user_prompts*/ true,
            "tty".to_string(),
            SessionSource::Cli,
        )
        .with_auth_env(auth_env_metadata());
        let root_span = tracing::info_span!("root");
        let _root_guard = root_span.enter();
        manager.sse_event_failed(
            Some("response.failed"),
            std::time::Duration::from_millis(2),
            &"private SSE diagnostic",
        );
        manager.see_event_completed_failed(&"private completion diagnostic");
        let agent_identity_telemetry = AgentIdentityTelemetry {
            agent_id: "agent-runtime-ws-request".to_string(),
            task_id: "task-run-ws-request".to_string(),
        };
        manager.record_websocket_request(
            std::time::Duration::from_millis(23),
            Some("stream error"),
            /*connection_reused*/ true,
            Some(&agent_identity_telemetry),
        );
    });

    logger_provider.force_flush().expect("flush logs");
    tracer_provider.force_flush().expect("flush traces");

    let logs = log_exporter.get_emitted_logs().expect("log export");
    let request_log = find_log_by_event_name(&logs, "codex.websocket_request");
    let request_log_attrs = log_attributes(&request_log.record);
    assert_eq!(
        request_log_attrs
            .get("auth.connection_reused")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        request_log_attrs.get("error.message").map(String::as_str),
        Some("stream error")
    );
    assert_eq!(
        request_log_attrs
            .get("auth.env_openai_api_key_present")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        request_log_attrs.get("auth.agent_id").map(String::as_str),
        Some("agent-runtime-ws-request")
    );
    assert_eq!(
        request_log_attrs.get("auth.task_id").map(String::as_str),
        Some("task-run-ws-request")
    );

    let sse_logs: Vec<_> = logs
        .iter()
        .map(|log| log_attributes(&log.record))
        .filter(|attrs| attrs.get("event.name").map(String::as_str) == Some("codex.sse_event"))
        .map(|attrs| attrs["error.message"].clone())
        .collect();
    assert_eq!(
        sse_logs,
        ["private SSE diagnostic", "private completion diagnostic"]
    );
    let spans = span_exporter.get_finished_spans().expect("span export");
    let sse_traces: Vec<_> = spans[0]
        .events
        .events
        .iter()
        .map(span_event_attributes)
        .filter(|attrs| attrs.get("event.name").map(String::as_str) == Some("codex.sse_event"))
        .collect();
    assert_eq!(sse_traces.len(), 2);
    for attrs in sse_traces {
        assert_eq!(attrs.get("error.present").map(String::as_str), Some("true"));
        assert!(!attrs.contains_key("error.message"));
        assert!(!attrs.values().any(|value| value.contains("private")));
    }

    let request_trace_event =
        find_span_event_by_name_attr(&spans[0].events.events, "codex.websocket_request");
    let request_trace_attrs = span_event_attributes(request_trace_event);
    assert_eq!(
        request_trace_attrs.get("error.present").map(String::as_str),
        Some("true")
    );
    for key in ["error.message", "endpoint", "auth.error", "auth.error_code"] {
        assert!(
            !request_trace_attrs.contains_key(key),
            "unexpected trace diagnostic: {key}"
        );
    }
    assert_eq!(
        request_trace_attrs
            .get("auth.connection_reused")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        request_trace_attrs
            .get("auth.env_provider_key_present")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        request_trace_attrs.get("auth.agent_id").map(String::as_str),
        Some("agent-runtime-ws-request")
    );
    assert_eq!(
        request_trace_attrs.get("auth.task_id").map(String::as_str),
        Some("task-run-ws-request")
    );
}

#[test]
fn otel_export_routing_policy_classifies_websocket_upgrade_as_success() {
    let exporter = InMemoryLogExporter::default();
    let provider = SdkLoggerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry().with(
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&provider)
            .with_filter(filter_fn(OtelProvider::log_export_filter)),
    );
    tracing::subscriber::with_default(subscriber, || {
        let manager = SessionTelemetry::new(
            ThreadId::new(),
            "model",
            "model",
            None,
            None,
            None,
            "test".to_string(),
            false,
            "test".to_string(),
            SessionSource::Cli,
        );
        for (status, error) in [
            (Some(101), None),
            (Some(200), None),
            (None, None),
            (Some(401), None),
            (Some(101), Some("upgrade failed")),
        ] {
            manager.record_websocket_connect(
                std::time::Duration::ZERO,
                status,
                error,
                false,
                None,
                false,
                None,
                None,
                "/responses",
                false,
                None,
                None,
                None,
                None,
                None,
            );
        }
    });
    let logs = exporter.get_emitted_logs().expect("export logs");
    let results: Vec<_> = logs
        .iter()
        .map(|log| log_attributes(&log.record)["success"].clone())
        .collect();
    assert_eq!(results, ["true", "true", "true", "false", "false"]);
}

#[test]
fn otel_export_routing_policy_trace_only_prompt_preserves_unicode_character_count() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("trace-only"))
            .with_filter(filter_fn(OtelProvider::trace_export_filter)),
    );
    tracing::subscriber::with_default(subscriber, || {
        let manager = SessionTelemetry::new(
            ThreadId::new(),
            "model",
            "model",
            None,
            None,
            None,
            "test".to_string(),
            true,
            "test".to_string(),
            SessionSource::Cli,
        );
        let span = tracing::info_span!("root");
        let _guard = span.enter();
        manager.user_prompt(&[UserInput::Text {
            text: "é🦀界".to_string(),
            text_elements: Vec::new(),
        }]);
    });
    let spans = exporter.get_finished_spans().expect("export traces");
    assert_eq!(spans.len(), 1);
    let attrs = span_event_attributes(find_span_event_by_name_attr(
        &spans[0].events.events,
        "codex.user_prompt",
    ));
    assert_eq!(attrs.get("prompt_length").map(String::as_str), Some("3"));
    assert_eq!(attrs.get("text_input_count").map(String::as_str), Some("1"));
    assert!(!attrs.contains_key("prompt"));
}

#[test]
fn otel_export_routing_policy_component_ids_enforce_version_bounds_in_exported_logs() {
    let exporter = InMemoryLogExporter::default();
    let provider = SdkLoggerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry().with(
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&provider)
            .with_filter(filter_fn(OtelProvider::log_export_filter)),
    );
    tracing::subscriber::with_default(subscriber, || {
        let manager = SessionTelemetry::new(
            ThreadId::new(),
            "model",
            "model",
            None,
            None,
            None,
            "test".to_string(),
            false,
            "test".to_string(),
            SessionSource::Cli,
        );
        for version in ["65535", "65536", "000001"] {
            manager.model_context_component(&codex_otel::ModelContextComponentTelemetry {
                sampling_request_id: "request".to_string(),
                attempt_id: "attempt".to_string(),
                retry_index: 0,
                kind: "repository".to_string(),
                contract_version: 1,
                semantic_id: format!("repository:v{version}:0123456789abcdef01234567"),
                content_hash: "0123456789abcdef01234567".to_string(),
                serialized_bytes: 42,
                approx_tokens: 10,
                active: true,
                disposition: "included".to_string(),
                local_reused: false,
                baseline_generation: None,
                provider_baseline: codex_otel::ModelAttemptProviderBaseline::FreshFullReplay,
                previous_response_id_present: false,
                local_projection_policy_active: false,
                fresh_response_id_established: false,
            });
        }
    });
    let logs = exporter.get_emitted_logs().expect("export logs");
    assert_eq!(logs.len(), 3);
    let ids: Vec<_> = logs
        .iter()
        .map(|log| log_attributes(&log.record).get("semantic_id").cloned())
        .collect();
    assert_eq!(
        ids,
        [
            Some("repository:v65535:0123456789abcdef01234567".to_string()),
            None,
            None
        ]
    );
    for log in logs {
        assert_eq!(
            log_attributes(&log.record)
                .get("serialized_bytes")
                .map(String::as_str),
            Some("42")
        );
    }
}
