use crate::error::ApiError;
use codex_client::Request;
use codex_client::RequestTelemetry;
use codex_client::Response;
use codex_client::RetryPolicy;
use codex_client::StreamResponse;
use codex_client::TransportError;
use codex_client::TransportPhase;
use codex_client::TransportPhaseObservation;
use codex_client::run_with_retry;
use codex_client::run_with_retry_non_idempotent;
use http::StatusCode;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Error;
use tokio_tungstenite::tungstenite::Message;

/// Generic telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsePollPhase {
    FirstEvent,
    SubsequentEvent,
}

impl SsePollPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FirstEvent => "first_event",
            Self::SubsequentEvent => "subsequent_event",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseCleanupOutcome {
    CompletedAndDrained,
    CompletedDrainTimeout,
    CompletedDrainError,
    ConsumerCancelled,
    IdleTimeout,
    ProtocolError,
    ResponseError,
    TransportError,
    CarrierEofBeforeCompleted,
}

impl SseCleanupOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompletedAndDrained => "completed_and_drained",
            Self::CompletedDrainTimeout => "completed_drain_timeout",
            Self::CompletedDrainError => "completed_drain_error",
            Self::ConsumerCancelled => "consumer_cancelled",
            Self::IdleTimeout => "idle_timeout",
            Self::ProtocolError => "protocol_error",
            Self::ResponseError => "response_error",
            Self::TransportError => "transport_error",
            Self::CarrierEofBeforeCompleted => "carrier_eof_before_completed",
        }
    }
}

/// Content-free, admitted provider control event. Receipt time is NOT server execution time.
#[derive(Debug)]
pub struct ProviderObservation {
    pub phase: &'static str,
    pub response_id: Option<String>,
    pub created_at: Option<f64>,
    pub completed_at: Option<f64>,
    pub safety_buffering: bool,
    pub invalid_source_fields: bool,
}

pub trait SseTelemetry: Send + Sync {
    fn on_provider_observation(&self, _observation: ProviderObservation) {}

    fn on_sse_poll(
        &self,
        result: &Result<
            Option<
                Result<
                    eventsource_stream::Event,
                    eventsource_stream::EventStreamError<TransportError>,
                >,
            >,
            tokio::time::error::Elapsed,
        >,
        duration: Duration,
    );

    fn on_sse_event(
        &self,
        _kind: &str,
        _duration: Duration,
        _error: Option<&dyn std::fmt::Display>,
    ) {
    }

    fn on_sse_phase(&self, _phase: SsePollPhase, _ordinal: u64, _duration: Duration) {}

    fn on_sse_cleanup(&self, _outcome: SseCleanupOutcome, _duration: Duration) {}
}

/// Telemetry for Responses WebSocket transport.
pub trait WebsocketTelemetry: Send + Sync {
    fn on_provider_observation(&self, _observation: ProviderObservation) {}

    /// Fresh parsed response evidence; never called for replayed handshake metadata.
    fn on_response_event(&self) {}
    /// Stable local connection correlation, not a provider receipt or credential.
    fn logical_transport_id(&self) -> Option<&str> {
        None
    }

    fn on_ws_request(&self, duration: Duration, error: Option<&ApiError>, connection_reused: bool);

    fn on_ws_event(
        &self,
        result: &Result<Option<Result<Message, Error>>, ApiError>,
        duration: Duration,
    );
}

pub(crate) trait WithStatus {
    fn status(&self) -> StatusCode;

    fn headers(&self) -> Option<&http::HeaderMap> {
        None
    }
}

fn http_status(err: &TransportError) -> Option<StatusCode> {
    match err {
        TransportError::Http { status, .. } => Some(*status),
        _ => None,
    }
}

impl WithStatus for Response {
    fn status(&self) -> StatusCode {
        self.status
    }

    fn headers(&self) -> Option<&http::HeaderMap> {
        Some(&self.headers)
    }
}

impl WithStatus for StreamResponse {
    fn status(&self) -> StatusCode {
        self.status
    }

    fn headers(&self) -> Option<&http::HeaderMap> {
        Some(&self.headers)
    }
}

pub(crate) async fn run_with_request_telemetry<T, F, Fut>(
    policy: RetryPolicy,
    telemetry: Option<Arc<dyn RequestTelemetry>>,
    make_request: impl FnMut() -> Request,
    send: F,
) -> Result<T, TransportError>
where
    T: WithStatus,
    F: Clone + Fn(Request) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    run_with_retry(policy, make_request, move |req, attempt| {
        observe_request_attempt(telemetry.clone(), send.clone(), req, attempt)
    })
    .await
}

pub(crate) async fn run_with_request_telemetry_non_idempotent<T, F, Fut>(
    policy: RetryPolicy,
    telemetry: Option<Arc<dyn RequestTelemetry>>,
    make_request: impl FnMut() -> Request,
    send: F,
) -> Result<T, TransportError>
where
    T: WithStatus,
    F: Clone + Fn(Request, u64) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    run_with_retry_non_idempotent(policy, make_request, move |req, attempt| {
        let send = send.clone();
        observe_request_attempt(
            telemetry.clone(),
            move |req| send(req, attempt),
            req,
            attempt,
        )
    })
    .await
}

async fn observe_request_attempt<T, F, Fut>(
    telemetry: Option<Arc<dyn RequestTelemetry>>,
    send: F,
    req: Request,
    attempt: u64,
) -> Result<T, TransportError>
where
    T: WithStatus,
    F: Fn(Request) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    let prepared_body_len = req.prepared_body_len();
    let start = Instant::now();
    struct ObservationGuard {
        telemetry: Option<Arc<dyn RequestTelemetry>>,
        attempt: u64,
        start: Instant,
    }
    impl Drop for ObservationGuard {
        fn drop(&mut self) {
            if let Some(telemetry) = self.telemetry.take() {
                telemetry.on_request_cancelled(self.attempt, self.start.elapsed());
            }
        }
    }
    let mut observation = ObservationGuard {
        telemetry: telemetry.clone(),
        attempt,
        start,
    };
    let (result, timing) = if telemetry.is_some() {
        codex_http_client::capture_transport_timing(send(req)).await
    } else {
        (
            send(req).await,
            codex_http_client::TransportTiming::default(),
        )
    };
    let duration = start.elapsed();
    observation.telemetry = None;
    if let Some(t) = telemetry.as_ref() {
        let headers = match &result {
            Ok(response) => response.headers(),
            Err(TransportError::Http { headers, .. }) => headers.as_ref(),
            Err(_) => None,
        };
        emit_transport_phases(t.as_ref(), attempt, &timing);
        t.on_transport_phase(
            attempt,
            TransportPhaseObservation {
                phase: TransportPhase::RequestUpload,
                duration: None,
                wire_bytes: prepared_body_len,
                provenance: "prepared_request_body",
                unavailable_reason: Some("upload timing is opaque inside reqwest"),
            },
        );
        t.on_transport_phase_with_timing(
            attempt,
            TransportPhaseObservation {
                phase: TransportPhase::ResponseHeaders,
                duration: timing.response_headers,
                wire_bytes: None,
                provenance: if timing.response_headers.is_some() {
                    "transport_send_to_response_headers"
                } else {
                    "request_attempt_boundary"
                },
                unavailable_reason: timing.response_headers.is_none().then_some(
                    "attempt includes authentication and may include response body reads",
                ),
            },
            &timing,
            headers,
        );
        let (status, err) = match &result {
            Ok(resp) => (Some(resp.status()), None),
            Err(err) => (http_status(err), Some(err)),
        };
        t.on_request(attempt, status, err, duration);
    }
    result
}

fn emit_transport_phases(
    telemetry: &dyn RequestTelemetry,
    attempt: u64,
    timing: &codex_http_client::TransportTiming,
) {
    for (phase, reason) in [
        (
            TransportPhase::EndpointResolution,
            "endpoint resolved before the transport observer",
        ),
        (
            TransportPhase::ProxyResolution,
            "proxy selection is opaque inside reqwest",
        ),
        (
            TransportPhase::ClientPoolSelection,
            "client selection occurs before the transport observer",
        ),
        (
            TransportPhase::DnsLookup,
            "dns timing is opaque inside reqwest",
        ),
        (
            TransportPhase::TcpConnect,
            "tcp timing is opaque inside reqwest",
        ),
        (
            TransportPhase::TlsHandshake,
            "tls timing is opaque inside reqwest",
        ),
        (
            TransportPhase::ConnectionReuse,
            "socket reuse is opaque inside reqwest",
        ),
    ] {
        let (duration, provenance) = match phase {
            TransportPhase::ProxyResolution => (timing.proxy_resolution, "route_resolver_boundary"),
            TransportPhase::ClientPoolSelection => {
                (timing.client_pool_selection, "route_client_pool")
            }
            _ => (None, "reqwest_transport_boundary"),
        };
        telemetry.on_transport_phase(
            attempt,
            TransportPhaseObservation {
                phase,
                duration,
                wire_bytes: None,
                provenance,
                unavailable_reason: duration.is_none().then_some(reason),
            },
        );
    }
}

