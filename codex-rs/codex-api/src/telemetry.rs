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
    ConsumerCancelled,
    IdleTimeout,
    ProtocolError,
    TransportError,
    CarrierEofBeforeCompleted,
}

impl SseCleanupOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompletedAndDrained => "completed_and_drained",
            Self::CompletedDrainTimeout => "completed_drain_timeout",
            Self::ConsumerCancelled => "consumer_cancelled",
            Self::IdleTimeout => "idle_timeout",
            Self::ProtocolError => "protocol_error",
            Self::TransportError => "transport_error",
            Self::CarrierEofBeforeCompleted => "carrier_eof_before_completed",
        }
    }
}

pub trait SseTelemetry: Send + Sync {
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
    fn on_ws_request(&self, duration: Duration, error: Option<&ApiError>, connection_reused: bool);

    fn on_ws_event(
        &self,
        result: &Result<Option<Result<Message, Error>>, ApiError>,
        duration: Duration,
    );
}

pub(crate) trait WithStatus {
    fn status(&self) -> StatusCode;
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
}

impl WithStatus for StreamResponse {
    fn status(&self) -> StatusCode {
        self.status
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
    F: Clone + Fn(Request) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    run_with_retry_non_idempotent(policy, make_request, move |req, attempt| {
        observe_request_attempt(telemetry.clone(), send.clone(), req, attempt)
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
    if let Some(t) = telemetry.as_ref() {
        emit_unavailable_transport_phases(t.as_ref(), attempt);
        t.on_transport_phase(
            attempt,
            TransportPhaseObservation {
                phase: TransportPhase::RequestUpload,
                duration: None,
                wire_bytes: req.prepared_body_len(),
                provenance: "prepared_request_body",
                unavailable_reason: Some("upload timing is opaque inside reqwest"),
            },
        );
    }
    let start = Instant::now();
    let result = send(req).await;
    if let Some(t) = telemetry.as_ref() {
        t.on_transport_phase(
            attempt,
            TransportPhaseObservation {
                phase: TransportPhase::ResponseHeaders,
                duration: Some(start.elapsed()),
                wire_bytes: None,
                provenance: "http_send_until_response_headers",
                unavailable_reason: None,
            },
        );
        let (status, err) = match &result {
            Ok(resp) => (Some(resp.status()), None),
            Err(err) => (http_status(err), Some(err)),
        };
        t.on_request(attempt, status, err, start.elapsed());
    }
    result
}

fn emit_unavailable_transport_phases(telemetry: &dyn RequestTelemetry, attempt: u64) {
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
        telemetry.on_transport_phase(
            attempt,
            TransportPhaseObservation {
                phase,
                duration: None,
                wire_bytes: None,
                provenance: "reqwest_transport_boundary",
                unavailable_reason: Some(reason),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_client::RetryOn;
    use http::HeaderMap;
    use http::Method;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingTelemetry {
        observations: Mutex<Vec<(u64, TransportPhaseObservation)>>,
    }

    impl RequestTelemetry for RecordingTelemetry {
        fn on_transport_phase(&self, attempt: u64, observation: TransportPhaseObservation) {
            self.observations
                .lock()
                .expect("telemetry mutex poisoned")
                .push((attempt, observation));
        }

        fn on_request(
            &self,
            _attempt: u64,
            _status: Option<StatusCode>,
            _error: Option<&TransportError>,
            _duration: Duration,
        ) {
        }
    }

    #[tokio::test]
    async fn transport_phase_telemetry_reports_opaque_stages_without_fabricated_timings() {
        let recorder = Arc::new(RecordingTelemetry::default());
        let telemetry: Arc<dyn RequestTelemetry> = recorder.clone();
        let policy = RetryPolicy {
            max_retries: 0,
            base_delay: Duration::ZERO,
            retry_on: RetryOn {
                retry_429: true,
                retry_5xx: true,
                retry_transport: true,
            },
        };
        let result = run_with_request_telemetry(
            policy,
            Some(telemetry),
            || {
                Request::new(Method::POST, "https://example.test/responses".to_string())
                    .with_raw_body("abc")
            },
            |_request| async {
                Ok(Response {
                    status: StatusCode::OK,
                    headers: HeaderMap::new(),
                    body: bytes::Bytes::new(),
                })
            },
        )
        .await;
        assert!(result.is_ok());

        let observations = recorder
            .observations
            .lock()
            .expect("telemetry mutex poisoned");
        assert_eq!(observations.len(), 9);
        assert!(observations[..7].iter().all(|(_, observation)| {
            observation.duration.is_none() && observation.unavailable_reason.is_some()
        }));
        assert_eq!(observations[7].1.phase, TransportPhase::RequestUpload);
        assert_eq!(observations[7].1.wire_bytes, Some(3));
        assert_eq!(observations[8].1.phase, TransportPhase::ResponseHeaders);
        assert!(observations[8].1.duration.is_some());
    }
}
