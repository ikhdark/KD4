use crate::rate_limits::RateLimitError;
use codex_client::TransportError;
use codex_http_client::RetryAfter;
use http::StatusCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("api error {status}: {message}")]
    Api { status: StatusCode, message: String },
    #[error("stream error: {0}")]
    Stream(String),
    #[error("{message}")]
    WebsocketClosed { code: Option<u16>, message: String },
    #[error("{0}")]
    IncompleteResponse(Box<codex_protocol::error::IncompleteResponse>),
    #[error("{source}")]
    TerminalResponse {
        source: Box<ApiError>,
        response_id: Option<String>,
        token_usage: Option<codex_protocol::protocol::TokenUsage>,
    },
    #[error("provider error {code:?}: {message}")]
    ProviderFailure {
        code: Option<String>,
        message: String,
    },
    #[error("context window exceeded")]
    ContextWindowExceeded,
    #[error("quota exceeded")]
    QuotaExceeded,
    #[error("usage not included")]
    UsageNotIncluded,
    #[error("retryable error: {message}")]
    Retryable {
        message: String,
        delay: Option<RetryAfter>,
    },
    #[error("websocket request rejected: {message}")]
    WebsocketRejected {
        reason: codex_protocol::error::WebsocketRecoveryReason,
        message: String,
        delay: Option<RetryAfter>,
    },
    #[error("rate limit: {0}")]
    RateLimit(String),
    #[error("invalid request: {message}")]
    InvalidRequest { message: String },
    #[error("cyber policy: {message}")]
    CyberPolicy { message: String },
    #[error("server overloaded")]
    ServerOverloaded { retry_after: Option<RetryAfter> },
}

/// Marks a mid-response WebSocket close whose code invites another connection.
///
/// A server that closes with `1000`/`1001`/`1012`/`1013` is shedding load or
/// restarting, not rejecting this request: the same request on a new socket
/// normally succeeds. Only the close *code* distinguishes that from a rejection
/// such as `1008`, and the code is lost once the frame is formatted, so the
/// marker is attached at the close site. Rebuilding the stream costs one
/// connection; abandoning WebSockets degrades every later request of the
/// session, so the retry layer reconnects once before falling back.
const RECONNECTABLE_CLOSE_MARKER: &str = " (reconnectable close)";

impl ApiError {
    /// A complete frame violated the provider protocol; replay is not recovery.
    pub fn invalid_response(message: impl Into<String>) -> Self {
        Self::ProviderFailure {
            code: Some("invalid_response".to_string()),
            message: message.into(),
        }
    }

    /// True when this is a provider-silence timeout rather than a broken stream.
    ///
    /// A post-dispatch stall consumes the bounded generation retry budget; it
    /// is not proof that no provider work occurred.
    pub fn is_stream_idle_timeout(&self) -> bool {
        matches!(self, Self::Stream(message) if is_stream_idle_timeout_message(message))
    }

    /// Builds the stream error for a mid-response close that invites a retry.
    ///
    /// `message` keeps the human-readable close code and reason; the marker is
    /// appended so [`is_reconnectable_close_message`] can recover the decision
    /// after the frame itself is gone.
    pub fn reconnectable_close(message: impl std::fmt::Display) -> Self {
        Self::Stream(format!("{message}{RECONNECTABLE_CLOSE_MARKER}"))
    }
}

/// True when a stream-error message reports provider silence.
///
/// Separate from [`ApiError::is_stream_idle_timeout`] so a caller that has
/// already destructured the variant can classify without rebuilding the error.
pub fn is_stream_idle_timeout_message(message: &str) -> bool {
    // Match the producer's complete envelope, never quoted provider text such
    // as a policy-close reason that happens to mention a timeout.
    matches!(
        message,
        "timeout waiting for websocket dispatch queue"
            | "model progress timeout waiting for SSE"
            | "model progress timeout waiting for websocket"
    ) || [
        "idle timeout waiting for SSE after ",
        "idle timeout waiting for websocket after ",
    ]
    .iter()
    .any(|prefix| {
        message
            .strip_prefix(prefix)
            .and_then(|duration| duration.strip_suffix("ms"))
            .is_some_and(|duration| {
                !duration.is_empty() && duration.bytes().all(|byte| byte.is_ascii_digit())
            })
    })
}

/// True when a stream-error message reports a close that invites another
/// connection, so the transport should be rebuilt before it is abandoned.
pub fn is_reconnectable_close_message(message: &str) -> bool {
    message.ends_with(RECONNECTABLE_CLOSE_MARKER)
}

/// Removes the routing marker so it never reaches a user-visible message.
pub fn strip_reconnectable_close_marker(mut message: String) -> String {
    if message.ends_with(RECONNECTABLE_CLOSE_MARKER) {
        message.truncate(message.len() - RECONNECTABLE_CLOSE_MARKER.len());
    }
    message
}

impl From<RateLimitError> for ApiError {
    fn from(err: RateLimitError) -> Self {
        Self::RateLimit(err.to_string())
    }
}
