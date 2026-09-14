use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::env;
use std::sync::OnceLock;
use std::sync::RwLock;

use codex_protocol::protocol::W3cTraceContext;
use opentelemetry::Context;
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::TraceContextExt;
use opentelemetry::trace::TraceState;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tracing::Span;
use tracing::debug;
use tracing::warn;
use tracing_opentelemetry::OpenTelemetrySpanExt;

const TRACEPARENT_ENV_VAR: &str = "TRACEPARENT";
const TRACESTATE_ENV_VAR: &str = "TRACESTATE";
static TRACEPARENT_CONTEXT: OnceLock<Option<Context>> = OnceLock::new();

// Trace context propagation can happen outside the provider object, so configured
// tracestate lives beside the process-global tracer provider.
static TRACESTATE_ENTRIES: OnceLock<RwLock<BTreeMap<String, BTreeMap<String, String>>>> =
    OnceLock::new();

pub fn current_span_w3c_trace_context() -> Option<W3cTraceContext> {
    span_w3c_trace_context(&Span::current())
}

pub fn span_w3c_trace_context(span: &Span) -> Option<W3cTraceContext> {
    let context = span.context();
    if !context.span().span_context().is_valid() {
        return None;
    }

    let mut headers = HashMap::new();
    TraceContextPropagator::new().inject_context(&context, &mut headers);
    let configured_tracestate_guard = tracestate_entries()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    Some(W3cTraceContext {
        traceparent: headers.remove("traceparent"),
        tracestate: merge_tracestate_entries(
            context.span().span_context().trace_state(),
            &configured_tracestate_guard,
        ),
    })
}

/// Injects the W3C trace context for `span` into HTTP headers.
///
/// Existing `traceparent` and `tracestate` values are replaced so callers can
/// safely reuse a request header map while keeping the supplied span as the
/// source of truth.
pub fn inject_span_w3c_trace_headers(span: &Span, headers: &mut http::HeaderMap) -> bool {
    headers.remove("traceparent");
    headers.remove("tracestate");
    let Some(trace) = span_w3c_trace_context(span) else {
        return false;
    };
    match trace.traceparent {
        Some(traceparent) => {
            if let Ok(value) = http::HeaderValue::from_str(&traceparent) {
                headers.insert("traceparent", value);
            }
        }
        None => {
            headers.remove("traceparent");
        }
    }
    match trace.tracestate {
        Some(tracestate) => {
            if let Ok(value) = http::HeaderValue::from_str(&tracestate) {
                headers.insert("tracestate", value);
            }
        }
        None => {
            headers.remove("tracestate");
        }
    }
    true
}

pub(crate) fn set_tracestate_entries(
    entries: BTreeMap<String, BTreeMap<String, String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_tracestate_entries(&entries)?;
    let mut guard = tracestate_entries()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = entries;
    Ok(())
}

pub fn current_span_trace_id() -> Option<String> {
    let context = Span::current().context();
    let span = context.span();
    let span_context = span.span_context();
    if !span_context.is_valid() {
        return None;
    }

    Some(span_context.trace_id().to_string())
}

pub fn context_from_w3c_trace_context(trace: &W3cTraceContext) -> Option<Context> {
    context_from_trace_headers(trace.traceparent.as_deref(), trace.tracestate.as_deref())
}

pub fn set_parent_from_w3c_trace_context(span: &Span, trace: &W3cTraceContext) -> bool {
    if let Some(context) = context_from_w3c_trace_context(trace) {
        span.set_parent(context).is_ok()
    } else {
        false
    }
}

pub fn set_parent_from_context(span: &Span, context: Context) {
    let _ = span.set_parent(context);
}

pub fn traceparent_context_from_env() -> Option<Context> {
    TRACEPARENT_CONTEXT
        .get_or_init(load_traceparent_context)
        .clone()
}

pub(crate) fn context_from_trace_headers(
    traceparent: Option<&str>,
    tracestate: Option<&str>,
) -> Option<Context> {
    let traceparent = traceparent?;
    let mut headers = HashMap::new();
    headers.insert("traceparent".to_string(), traceparent.to_string());
    if let Some(tracestate) = tracestate {
        headers.insert("tracestate".to_string(), tracestate.to_string());
    }

    let context = TraceContextPropagator::new().extract(&headers);
    if !context.span().span_context().is_valid() {
        return None;
    }
    Some(context)
}

fn load_traceparent_context() -> Option<Context> {
    let traceparent = env::var(TRACEPARENT_ENV_VAR).ok()?;
    let tracestate = env::var(TRACESTATE_ENV_VAR).ok();

    match context_from_trace_headers(Some(&traceparent), tracestate.as_deref()) {
        Some(context) => {
            debug!("TRACEPARENT detected; continuing trace from parent context");
            Some(context)
        }
        None => {
            warn!("TRACEPARENT is set but invalid; ignoring trace context");
            None
        }
    }
}

fn tracestate_entries() -> &'static RwLock<BTreeMap<String, BTreeMap<String, String>>> {
    TRACESTATE_ENTRIES.get_or_init(|| RwLock::new(BTreeMap::new()))
}

fn merge_tracestate_entries(
    tracestate: &TraceState,
    configured_entries: &BTreeMap<String, BTreeMap<String, String>>,
) -> Option<String> {
    let mut trace_state = tracestate.clone();

    // TraceState::insert places members at the front. Reverse iteration keeps
    // deterministic map order while upserting fields inside configured members.
    for (key, fields) in configured_entries.iter().rev() {
        let value = merge_tracestate_member_fields(trace_state.get(key), fields);
        if !is_tracestate_member_key(key) || !is_header_safe_tracestate_member_value(&value) {
            warn!("ignoring invalid configured tracestate member update");
            continue;
        }
        trace_state = match trace_state.insert(key.clone(), value) {
            Ok(trace_state) => trace_state,
            Err(err) => {
                warn!("ignoring configured tracestate while propagating trace context: {err}");
                continue;
            }
        };
    }

    // The pinned SDK does not enforce all W3C constraints. Preserve valid
    // members in priority order, dropping rightmost entries beyond the limit.
    let mut seen = BTreeSet::new();
    let tracestate = trace_state
        .into_iter()
        .filter(|(key, value)| {
            is_tracestate_member_key(key)
                && is_header_safe_tracestate_member_value(value)
                && seen.insert(*key)
        })
        .take(32)
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    (!tracestate.is_empty()).then_some(tracestate)
}

/// Validates configured tracestate members before they are propagated in W3C trace context.
pub fn validate_tracestate_entries(
    entries: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if entries.len() > 32 {
        return Err(invalid_tracestate_config(
            "configured tracestate exceeds 32 members".to_string(),
        ));
    }
    let entries = entries
        .iter()
        .map(|(key, fields)| encode_tracestate_member_fields(key, fields))
        .collect::<Result<Vec<_>, _>>()?;
    TraceState::from_key_value(
        entries
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )
    .map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid configured tracestate: {err}"),
        )
    })?;
    Ok(())
}

/// Validates one configured tracestate member and its encoded field value.
pub fn validate_tracestate_member(
    member_key: &str,
    fields: &BTreeMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (key, value) = encode_tracestate_member_fields(member_key, fields)?;
    TraceState::from_key_value([(key.as_str(), value.as_str())]).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid configured tracestate: {err}"),
        )
    })?;
    Ok(())
}

fn encode_tracestate_member_fields(
    member_key: &str,
    fields: &BTreeMap<String, String>,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    if !is_tracestate_member_key(member_key) {
        return Err(invalid_tracestate_config(format!(
            "invalid configured tracestate member key {member_key:?}"
        )));
    }
    // Configured fields are encoded into one opaque tracestate member value.
    // Validate both the field grammar and the final header value so malformed
    // config cannot produce propagated trace context that downstream W3C
    // extractors reject.
    let mut encoded = Vec::with_capacity(fields.len());
    for (field_key, value) in fields {
        if !is_configured_tracestate_field_key(field_key) {
            return Err(invalid_tracestate_config(format!(
                "invalid configured tracestate field key {member_key}.{field_key}"
            )));
        }
        if !is_configured_tracestate_field_value(value) {
            return Err(invalid_tracestate_config(format!(
                "invalid configured tracestate value for {member_key}.{field_key}"
            )));
        }
        encoded.push(format!("{field_key}:{value}"));
    }
    let value = encoded.join(";");
    if !is_header_safe_tracestate_member_value(&value) {
        return Err(invalid_tracestate_config(format!(
            "invalid configured tracestate value for {member_key}"
        )));
    }
    Ok((member_key.to_string(), value))
}

fn is_configured_tracestate_field_key(field_key: &str) -> bool {
    !field_key.is_empty()
        && field_key
            .bytes()
            .all(|byte| matches!(byte, b'!'..=b'~') && !matches!(byte, b':' | b';' | b',' | b'='))
}

fn is_configured_tracestate_field_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| is_tracestate_member_value_byte(byte) && byte != b';')
}

fn is_header_safe_tracestate_member_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(is_tracestate_member_value_byte)
        && value.as_bytes().last().is_some_and(|byte| *byte != b' ')
}

fn is_tracestate_member_key(key: &str) -> bool {
    let valid_part = |part: &str, max_len: usize, allow_digit: bool| {
        !part.is_empty()
            && part.len() <= max_len
            && part
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_lowercase() || (allow_digit && b.is_ascii_digit()))
            && part.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'_' | b'-' | b'*' | b'/')
            })
    };
    match key.split_once('@') {
        Some((tenant, system)) => valid_part(tenant, 241, true) && valid_part(system, 14, false),
        None => valid_part(key, 256, false),
    }
}

fn is_tracestate_member_value_byte(byte: u8) -> bool {
    matches!(byte, b' '..=b'~') && !matches!(byte, b',' | b'=')
}

fn invalid_tracestate_config(message: String) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    ))
}

fn merge_tracestate_member_fields(
    existing: Option<&str>,
    configured_fields: &BTreeMap<String, String>,
) -> String {
    // W3C TraceState treats member values as opaque strings. The config models
    // values as semicolon-separated key:value fields so selected fields can be
    // upserted without replacing unrelated fields in the same member.
    let mut fields = Vec::new();
    let mut seen = BTreeSet::new();

    if let Some(existing) = existing {
        for field in existing.split(';').filter(|field| !field.is_empty()) {
            if let Some((field_key, _)) = field.split_once(':') {
                if let Some(value) = configured_fields.get(field_key) {
                    if seen.insert(field_key) {
                        fields.push(format!("{field_key}:{value}"));
                    }
                    continue;
                }
                seen.insert(field_key);
            }
            fields.push(field.to_string());
        }
    }

    fields.extend(
        configured_fields
            .iter()
            .filter(|(field_key, _)| !seen.contains(field_key.as_str()))
            .map(|(field_key, value)| format!("{field_key}:{value}")),
    );
    fields.join(";")
}

#[cfg(test)]
mod tests {
    use super::context_from_trace_headers;
    use super::context_from_w3c_trace_context;
    use super::current_span_trace_id;
    use codex_protocol::protocol::W3cTraceContext;
    use opentelemetry::trace::SpanId;
    use opentelemetry::trace::TraceContextExt;
    use opentelemetry::trace::TraceId;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use pretty_assertions::assert_eq;
    use tracing::trace_span;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    #[test]
    fn parses_valid_w3c_trace_context() {
        let trace_id = "00000000000000000000000000000001";
        let span_id = "0000000000000002";
        let context = context_from_w3c_trace_context(&W3cTraceContext {
            traceparent: Some(format!("00-{trace_id}-{span_id}-01")),
            tracestate: None,
        })
        .expect("trace context");

        let span = context.span();
        let span_context = span.span_context();
        assert_eq!(
            span_context.trace_id(),
            TraceId::from_hex(trace_id).unwrap()
        );
        assert_eq!(span_context.span_id(), SpanId::from_hex(span_id).unwrap());
        assert!(span_context.is_remote());
    }

    #[test]
    fn invalid_traceparent_returns_none() {
        assert!(
            context_from_trace_headers(Some("not-a-traceparent"), /*tracestate*/ None).is_none()
        );
    }

    #[test]
    fn missing_traceparent_returns_none() {
        assert!(
            context_from_w3c_trace_context(&W3cTraceContext {
                traceparent: None,
                tracestate: Some("vendor=value".to_string()),
            })
            .is_none()
        );
    }

    #[test]
    fn current_span_trace_id_returns_hex_trace_id() {
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("codex-otel-tests");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
        let _guard = subscriber.set_default();

        let span = trace_span!("test_span");
        let _entered = span.enter();
        let trace_id = current_span_trace_id().expect("trace id");

        assert_eq!(trace_id.len(), 32);
        assert!(trace_id.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(trace_id, "00000000000000000000000000000000");
    }

    #[test]
    fn reused_headers_follow_supplied_span_and_clear_for_invalid_span() {
        let provider = SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("headers")));
        let _guard = subscriber.set_default();
        let span = tracing::info_span!("request");
        let mut headers = http::HeaderMap::new();
        headers.insert("traceparent", "stale".parse().unwrap());
        headers.insert("tracestate", "stale=value".parse().unwrap());
        headers.insert("authorization", "preserved".parse().unwrap());
        assert!(super::inject_span_w3c_trace_headers(&span, &mut headers));
        let expected = super::span_w3c_trace_context(&span).unwrap();
        assert_eq!(
            headers["traceparent"].to_str().unwrap(),
            expected.traceparent.unwrap()
        );
        assert!(!headers.contains_key("tracestate"));
        headers.insert("tracestate", "stale=value".parse().unwrap());
        assert!(!super::inject_span_w3c_trace_headers(
            &tracing::Span::none(),
            &mut headers
        ));
        assert!(!headers.contains_key("traceparent"));
        assert!(!headers.contains_key("tracestate"));
        assert_eq!(headers["authorization"], "preserved");
    }

    #[test]
    fn parenting_reports_rejection_after_span_start() {
        use tracing_opentelemetry::OpenTelemetrySpanExt;
        let provider = SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("parenting")));
        let _guard = subscriber.set_default();
        let trace = W3cTraceContext {
            traceparent: Some(
                "00-00000000000000000000000000000001-0000000000000002-01".to_string(),
            ),
            tracestate: None,
        };
        assert!(!super::set_parent_from_w3c_trace_context(
            &tracing::Span::none(),
            &trace
        ));
        let span = tracing::info_span!("request");
        assert!(super::set_parent_from_w3c_trace_context(&span, &trace));
        let context = span.context();
        assert_eq!(
            context.span().span_context().trace_id(),
            TraceId::from_hex("00000000000000000000000000000001").unwrap()
        );
        let different_parent = W3cTraceContext {
            traceparent: Some(
                "00-00000000000000000000000000000003-0000000000000004-01".to_string(),
            ),
            tracestate: None,
        };
        assert!(!super::set_parent_from_w3c_trace_context(
            &span,
            &different_parent
        ));
        assert_eq!(
            span.context().span().span_context().trace_id(),
            context.span().span_context().trace_id()
        );
    }

    #[test]
    fn configured_tracestate_enforces_key_value_and_member_limits() {
        use std::collections::BTreeMap;
        let fields = BTreeMap::from([("field".to_string(), "value".to_string())]);
        for key in ["", "123", "tenant@", "tenant@1system", "a@@b"] {
            assert!(
                super::validate_tracestate_member(key, &fields).is_err(),
                "{key:?}"
            );
        }
        for key in ["vendor", "1tenant@vendor"] {
            super::validate_tracestate_member(key, &fields).unwrap();
        }
        assert!(super::validate_tracestate_member("vendor", &BTreeMap::new()).is_err());
        let oversized = BTreeMap::from([("f".to_string(), "v".repeat(255))]);
        assert!(super::validate_tracestate_member("vendor", &oversized).is_err());
        let mut entries: BTreeMap<_, _> = (0..32)
            .map(|i| (format!("vendor{i}"), fields.clone()))
            .collect();
        super::validate_tracestate_entries(&entries).unwrap();
        entries.insert("extra".to_string(), fields);
        assert!(super::validate_tracestate_entries(&entries).is_err());
    }

    #[test]
    fn tracestate_merge_preserves_failed_member_and_applies_unrelated_update() {
        use opentelemetry::trace::TraceState;
        use std::collections::BTreeMap;
        let original = format!("keep:{}", "x".repeat(245));
        let state =
            TraceState::from_key_value([("z", original.as_str()), ("other", "value")]).unwrap();
        let entries = BTreeMap::from([
            (
                "z".to_string(),
                BTreeMap::from([("extra".to_string(), "too-long".to_string())]),
            ),
            (
                "a".to_string(),
                BTreeMap::from([("f".to_string(), "ok".to_string())]),
            ),
        ]);
        assert_eq!(
            super::merge_tracestate_entries(&state, &entries),
            Some(format!("a=f:ok,z={original},other=value"))
        );
        assert_eq!(
            super::merge_tracestate_entries(&state, &BTreeMap::new()),
            Some(format!("z={original},other=value"))
        );
    }

    #[test]
    fn tracestate_merge_caps_final_members_and_discards_invalid_native_members() {
        use opentelemetry::trace::TraceState;
        use std::collections::BTreeMap;
        let mut pairs: Vec<_> = (0..32)
            .map(|i| (format!("v{i}"), "value".to_string()))
            .collect();
        pairs.insert(0, ("".to_string(), "value".to_string()));
        pairs.insert(0, ("empty".to_string(), "".to_string()));
        let state = TraceState::from_key_value(pairs).unwrap();
        let entries = BTreeMap::from([(
            "first".to_string(),
            BTreeMap::from([("f".to_string(), "ok".to_string())]),
        )]);
        let merged = super::merge_tracestate_entries(&state, &entries).unwrap();
        let expected = std::iter::once("first=f:ok".to_string())
            .chain((0..31).map(|i| format!("v{i}=value")))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(merged, expected);
    }
    #[test]
    fn tracestate_merge_preserves_original_when_field_order_creates_trailing_space() {
        use opentelemetry::trace::TraceState;
        use std::collections::BTreeMap;
        let state =
            TraceState::from_key_value([("vendor", "z:old;a:old"), ("other", "keep")]).unwrap();
        let entries = BTreeMap::from([(
            "vendor".to_string(),
            BTreeMap::from([
                ("a".to_string(), "x ".to_string()),
                ("z".to_string(), "y".to_string()),
            ]),
        )]);
        super::validate_tracestate_entries(&entries).expect("sorted configuration is valid");
        assert_eq!(
            super::merge_tracestate_entries(&state, &entries),
            Some("vendor=z:old;a:old,other=keep".to_string())
        );
    }
}
