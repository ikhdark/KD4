use base64::Engine;
use codex_api::ApiError;
use codex_api::TransportError;

const REQUEST_ID_HEADER: &str = "x-request-id";
const OAI_REQUEST_ID_HEADER: &str = "x-oai-request-id";
const CF_RAY_HEADER: &str = "cf-ray";
const AUTH_ERROR_HEADER: &str = "x-openai-authorization-error";
const X_ERROR_JSON_HEADER: &str = "x-error-json";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResponseDebugContext {
    pub request_id: Option<String>,
    pub cf_ray: Option<String>,
    pub auth_error: Option<String>,
    pub auth_error_code: Option<String>,
}

pub fn extract_response_debug_context(transport: &TransportError) -> ResponseDebugContext {
    let mut context = ResponseDebugContext::default();

    let TransportError::Http { headers, .. } = transport else {
        return context;
    };

    let extract_header = |name: &str| {
        headers
            .as_ref()
            .and_then(|headers| headers.get(name))
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };

    context.request_id =
        extract_header(REQUEST_ID_HEADER).or_else(|| extract_header(OAI_REQUEST_ID_HEADER));
    context.cf_ray = extract_header(CF_RAY_HEADER);
    context.auth_error = extract_header(AUTH_ERROR_HEADER);
    context.auth_error_code = extract_header(X_ERROR_JSON_HEADER).and_then(|encoded| {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?;
        let parsed = serde_json::from_slice::<serde_json::Value>(&decoded).ok()?;
        parsed
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    });

    context
}

pub fn extract_response_debug_context_from_api_error(error: &ApiError) -> ResponseDebugContext {
    match error {
        ApiError::Transport(transport) => extract_response_debug_context(transport),
        ApiError::TerminalResponse { source, .. } => extract_response_debug_context_from_api_error(source),
        _ => ResponseDebugContext::default(),
    }
}

pub fn telemetry_transport_error_message(error: &TransportError) -> String {
    match error {
        TransportError::Http { status, .. } => format!("http {}", status.as_u16()),
        TransportError::RetryLimit => "retry limit reached".to_string(),
        TransportError::Timeout => "timeout".to_string(),
        TransportError::ConnectionTimeout => "connection establishment timeout".to_string(),
        TransportError::Connection(err) => err.to_string(),
        TransportError::Network(err) => err.to_string(),
        TransportError::Build(err) => err.to_string(),
        TransportError::PreDispatch(err) => err.to_string(),
        TransportError::ResponseTooLarge { limit } => format!("response body exceeds {limit} bytes"),
    }
}

pub fn telemetry_api_error_message(error: &ApiError) -> String {
    match error {
        ApiError::Transport(transport) => telemetry_transport_error_message(transport),
        ApiError::Api { status, .. } => format!("api error {}", status.as_u16()),
        ApiError::Stream(err) => err.to_string(),
        ApiError::WebsocketClosed { code, .. } => format!("websocket closed: {code:?}"),
        ApiError::ContextWindowExceeded => "context window exceeded".to_string(),
        ApiError::QuotaExceeded => "quota exceeded".to_string(),
        ApiError::UsageNotIncluded => "usage not included".to_string(),
        ApiError::Retryable { .. } => "retryable error".to_string(),
        ApiError::WebsocketRejected { reason, .. } => format!("websocket rejected: {reason:?}"),
        ApiError::RateLimit(_) => "rate limit".to_string(),
        ApiError::InvalidRequest { .. } => "invalid request".to_string(),
        ApiError::CyberPolicy { .. } => "cyber policy".to_string(),
        ApiError::ServerOverloaded { .. } => "server overloaded".to_string(),
        ApiError::IncompleteResponse(_) => "incomplete response".to_string(),
        ApiError::TerminalResponse { source, .. } => telemetry_api_error_message(source),
        ApiError::ProviderFailure { .. } => "provider failure".to_string(),
    }
}

