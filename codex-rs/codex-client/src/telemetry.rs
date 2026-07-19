use codex_http_client::TransportError;
use http::StatusCode;
use std::time::Duration;

/// API-specific request telemetry.
pub trait RequestTelemetry: Send + Sync {
    /// Called once for each operation attempt.
    ///
    /// `attempt` is zero-based. The callback can observe failures before any
    /// bytes are sent, such as an authentication failure.
    fn on_request(
        &self,
        attempt: u64,
        status: Option<StatusCode>,
        error: Option<&TransportError>,
        duration: Duration,
    );
}
