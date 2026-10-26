use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
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

    let context = TraceContextPropagator::new().extract_with_context(&Context::new(), &headers);
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
    // Merge into a plain list: `TraceState::insert` drops the rightmost member of
    // a full list, so a valid member could lose its slot to an invalid one that
    // the final filter discards.
    let mut members: VecDeque<(String, String)> = tracestate
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();

    // Configured members go in front. Reverse iteration keeps deterministic map
    // order while upserting fields inside configured members.
    for (key, fields) in configured_entries.iter().rev() {
        let existing = members.iter().position(|(member_key, _)| member_key == key);
        let value =
            merge_tracestate_member_fields(existing.map(|index| members[index].1.as_str()), fields);
        if !is_tracestate_member_key(key) || !is_header_safe_tracestate_member_value(&value) {
            warn!("ignoring invalid configured tracestate member update");
            continue;
        }
        if let Some(index) = existing {
            members.remove(index);
        }
        members.push_front((key.clone(), value));
    }

    // The pinned SDK does not enforce all W3C constraints. Preserve valid
    // members in priority order, dropping rightmost entries beyond the limit.
    let mut seen = BTreeSet::new();
    let tracestate = members
        .iter()
        .filter(|(key, value)| {
            is_tracestate_member_key(key)
                && is_header_safe_tracestate_member_value(value)
                && seen.insert(key.as_str())
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

