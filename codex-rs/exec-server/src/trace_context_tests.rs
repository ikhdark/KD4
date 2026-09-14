use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::prelude::*;

use super::current_trace_context_headers;

#[test]
fn creates_traceparent_header_from_current_span() {
    let provider = SdkTracerProvider::builder().build();
    let tracer = provider.tracer("exec-server-test");
    let subscriber =
        tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
    let _guard = subscriber.set_default();
    tracing::callsite::rebuild_interest_cache();
    assert!(current_trace_context_headers().is_empty());
    let mut previous_traceparent = None;
    for _ in 0..2 {
        let span = tracing::info_span!("outbound-request");
        let parent = codex_protocol::protocol::W3cTraceContext {
            traceparent: Some(
                "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01".to_string(),
            ),
            tracestate: Some("vendor=test".to_string()),
        };
        assert!(codex_otel::set_parent_from_w3c_trace_context(
            &span, &parent
        ));
        let expected = codex_otel::span_w3c_trace_context(&span).expect("span context");
        let headers = span.in_scope(current_trace_context_headers);
        let traceparent = headers.get("traceparent").expect("traceparent header");
        assert_eq!(
            traceparent.to_str().expect("valid header"),
            expected
                .traceparent
                .as_deref()
                .expect("expected traceparent")
        );
        assert_eq!(
            headers.get("tracestate").expect("tracestate header"),
            "vendor=test"
        );
        assert_ne!(Some(traceparent), previous_traceparent.as_ref());
        previous_traceparent = Some(traceparent.clone());
        assert!(current_trace_context_headers().is_empty());
    }
}
