use codex_http_client::TransportError;
use http::StatusCode;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPhase {
    EndpointResolution,
    ProxyResolution,
    ClientPoolSelection,
    DnsLookup,
    TcpConnect,
    TlsHandshake,
    ConnectionReuse,
    RequestUpload,
    ResponseHeaders,
}

impl TransportPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EndpointResolution => "endpoint_resolution",
            Self::ProxyResolution => "proxy_resolution",
            Self::ClientPoolSelection => "client_pool_selection",
            Self::DnsLookup => "dns_lookup",
            Self::TcpConnect => "tcp_connect",
            Self::TlsHandshake => "tls_handshake",
            Self::ConnectionReuse => "connection_reuse",
            Self::RequestUpload => "request_upload",
            Self::ResponseHeaders => "response_headers",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportPhaseObservation {
    pub phase: TransportPhase,
    pub duration: Option<Duration>,
    pub wire_bytes: Option<u64>,
    pub provenance: &'static str,
    pub unavailable_reason: Option<&'static str>,
}

/// API-specific request telemetry.
pub trait RequestTelemetry: Send + Sync {
    /// Called with truthful per-stage observations. Phases hidden inside the
    /// HTTP implementation are reported as unavailable instead of receiving a
    /// fabricated duration or reuse result.
    fn on_transport_phase(&self, _attempt: u64, _observation: TransportPhaseObservation) {}

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
