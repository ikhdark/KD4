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
    let prepared_body_len = req.prepared_body_len();
    let start = Instant::now();
    let (result, timing) = if telemetry.is_some() {
        codex_http_client::capture_transport_timing(send(req)).await
    } else {
        (send(req).await, codex_http_client::TransportTiming::default())
    };
    let duration = start.elapsed();
    if let Some(t) = telemetry.as_ref() {
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
        t.on_transport_phase(
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
            TransportPhase::ClientPoolSelection => (timing.client_pool_selection, "route_client_pool"),
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
        requests: Mutex<Vec<(u64, Option<StatusCode>, bool)>>,
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
            attempt: u64,
            status: Option<StatusCode>,
            error: Option<&TransportError>,
            _duration: Duration,
        ) {
            self.requests
                .lock()
                .expect("telemetry mutex poisoned")
                .push((attempt, status, error.is_some()));
        }
    }

    #[tokio::test]
    async fn failed_attempts_do_not_claim_response_header_timings() {
        for expected_status in [None, Some(StatusCode::FORBIDDEN)] {
            let recorder = Arc::new(RecordingTelemetry::default());
            let result: Result<Response, TransportError> = observe_request_attempt(
                Some(recorder.clone()),
                |_| async move {
                    Err(match expected_status {
                        Some(status) => TransportError::Http {
                            retry_after: None,
                            status,
                            url: None,
                            headers: None,
                            body: None,
                        },
                        None => TransportError::Network("connection closed".into()),
                    })
                },
                Request::new(Method::POST, "https://example.test/responses".into()),
                7,
            )
            .await;
            assert!(result.is_err());
            assert_eq!(
                *recorder.requests.lock().expect("telemetry mutex poisoned"),
                vec![(7, expected_status, true)]
            );
            let observations = recorder
                .observations
                .lock()
                .expect("telemetry mutex poisoned");
            let headers = observations
                .iter()
                .filter(|(_, observation)| observation.phase == TransportPhase::ResponseHeaders)
                .collect::<Vec<_>>();
            assert_eq!(headers.len(), 1);
            assert_eq!(headers[0].0, 7);
            assert_eq!(headers[0].1.duration, None);
            assert!(headers[0].1.unavailable_reason.is_some());
        }
    }

    #[tokio::test]
    async fn concrete_transport_reports_headers_and_route_phases_even_on_http_errors() {
        use codex_http_client::HttpTransport;
        let server = wiremock::MockServer::start().await;
        for status in [200, 429] {
            wiremock::Mock::given(wiremock::matchers::path(format!("/{status}")))
                .respond_with(wiremock::ResponseTemplate::new(status).set_body_string("body"))
                .expect(1)
                .mount(&server).await;
            let pool = codex_http_client::RouteAwareClientPool::new(
                codex_http_client::HttpClientFactory::new(codex_http_client::OutboundProxyPolicy::ReqwestDefault),
                codex_http_client::ClientRouteClass::Api,
            );
            let transport = codex_http_client::ReqwestTransport::from_client_pool(pool);
            let recorder = Arc::new(RecordingTelemetry::default());
            let start = Instant::now();
            let result = observe_request_attempt(
                Some(recorder.clone()),
                |request| async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    transport.execute(request).await
                },
                Request::new(Method::GET, format!("{}/{status}", server.uri())),
                3,
            ).await;
            assert_eq!(result.is_ok(), status == 200);
            let elapsed = start.elapsed();
            let observations = recorder.observations.lock().unwrap();
            assert_eq!(observations.len(), 9);
            for phase in [TransportPhase::ProxyResolution, TransportPhase::ClientPoolSelection, TransportPhase::ResponseHeaders] {
                let (attempt, observation) = observations.iter().find(|(_, item)| item.phase == phase).unwrap();
                assert_eq!(*attempt, 3);
                assert!(observation.duration.is_some(), "{phase:?}");
                assert!(observation.unavailable_reason.is_none());
            }
            let headers = observations.iter().find(|(_, item)| item.phase == TransportPhase::ResponseHeaders).unwrap().1;
            assert!(elapsed.saturating_sub(headers.duration.unwrap()) >= Duration::from_millis(50));
            let reuse = observations.iter().find(|(_, item)| item.phase == TransportPhase::ConnectionReuse).unwrap().1;
            assert!(reuse.duration.is_none());
            assert!(reuse.unavailable_reason.is_some());
        }
    }

    #[tokio::test]
    async fn request_duration_excludes_the_response_telemetry_callback() {
        struct SlowTelemetry {
            callback_duration: Mutex<Duration>,
            request_duration: Mutex<Option<Duration>>,
        }

        impl RequestTelemetry for SlowTelemetry {
            fn on_transport_phase(&self, _attempt: u64, observation: TransportPhaseObservation) {
                if observation.phase == TransportPhase::ResponseHeaders {
                    let start = Instant::now();
                    std::thread::sleep(Duration::from_millis(50));
                    *self.callback_duration.lock().unwrap() = start.elapsed();
                }
            }

            fn on_request(
                &self,
                _attempt: u64,
                _status: Option<StatusCode>,
                _error: Option<&TransportError>,
                duration: Duration,
            ) {
                *self.request_duration.lock().unwrap() = Some(duration);
            }
        }

        let recorder = Arc::new(SlowTelemetry {
            callback_duration: Mutex::new(Duration::ZERO),
            request_duration: Mutex::new(None),
        });
        let result = run_with_request_telemetry_non_idempotent(
            RetryPolicy {
                max_retries: 0,
                base_delay: Duration::ZERO,
                retry_on: RetryOn {
                    retry_429: false,
                    retry_5xx: false,
                    retry_transport: false,
                },
            },
            Some(recorder.clone()),
            || Request::new(Method::POST, "https://example.test/responses".into()),
            |_| async {
                Ok(Response {
                    status: StatusCode::OK,
                    headers: HeaderMap::new(),
                    body: bytes::Bytes::new(),
                })
            },
        )
        .await
        .expect("request succeeds");
        assert_eq!(result.status, StatusCode::OK);
        let measured = recorder
            .request_duration
            .lock()
            .unwrap()
            .expect("request timing");
        assert!(
            measured < *recorder.callback_duration.lock().unwrap(),
            "request duration included the telemetry callback: {measured:?}",
        );
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
        assert_eq!(observations[8].1.duration, None);
        assert_eq!(observations[8].1.provenance, "request_attempt_boundary");
        assert!(observations[8].1.unavailable_reason.is_some());
    }
}
