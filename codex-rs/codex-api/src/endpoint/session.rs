use crate::auth::SharedAuthProvider;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::telemetry::run_with_request_telemetry;
use crate::telemetry::run_with_request_telemetry_non_idempotent;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_client::RequestBody;
use codex_client::RequestCompression;
use codex_client::RequestTelemetry;
use codex_client::Response;
use codex_client::StreamResponse;
use codex_client::TransportError;
use http::HeaderMap;
use http::Method;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tracing::instrument;

async fn prepare_request(request: Request) -> Result<Request, TransportError> {
    prepare_request_with(request, Request::into_prepared).await
}

async fn prepare_request_with<F>(request: Request, prepare: F) -> Result<Request, TransportError>
where
    F: FnOnce(Request) -> Result<Request, String> + Send + 'static,
{
    if request.compression == RequestCompression::None {
        return prepare(request).map_err(TransportError::Build);
    }

    tokio::task::spawn_blocking(move || prepare(request))
        .await
        .map_err(|error| {
            TransportError::Build(format!("request preparation task failed: {error}"))
        })?
        .map_err(TransportError::Build)
}

pub(crate) struct EndpointSession<T: HttpTransport> {
    transport: T,
    provider: Provider,
    auth: SharedAuthProvider,
    request_telemetry: Option<Arc<dyn RequestTelemetry>>,
}

impl<T: HttpTransport> EndpointSession<T> {
    pub(crate) fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            transport,
            provider,
            auth,
            request_telemetry: None,
        }
    }

    pub(crate) fn with_request_telemetry(
        mut self,
        request: Option<Arc<dyn RequestTelemetry>>,
    ) -> Self {
        self.request_telemetry = request;
        self
    }

    pub(crate) fn provider(&self) -> &Provider {
        &self.provider
    }

    fn make_request(
        &self,
        method: &Method,
        path: &str,
        extra_headers: &HeaderMap,
        body: Option<RequestBody>,
    ) -> Request {
        let mut req = self.provider.build_request(method.clone(), path);
        req.headers.extend(extra_headers.clone());
        if let Some(body) = body {
            req.body = Some(body);
        }
        req
    }

    pub(crate) async fn execute(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<EncodedJsonBody>,
    ) -> Result<Response, ApiError> {
        self.execute_body_with(
            method,
            path,
            extra_headers,
            body.map(RequestBody::EncodedJson),
            |_| {},
        )
        .await
    }

    #[instrument(
        name = "endpoint_session.execute_non_idempotent",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn execute_non_idempotent(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<EncodedJsonBody>,
    ) -> Result<Response, ApiError> {
        let body = body.map(RequestBody::EncodedJson);
        let request = self.make_request(&method, path, &extra_headers, body);
        let request = prepare_request(request).await?;
        let make_request = || request.clone();

        let response = run_with_request_telemetry_non_idempotent(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    transport.execute(req).await
                }
            },
        )
        .await?;

        Ok(response)
    }

    #[instrument(
        name = "endpoint_session.execute_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn execute_with<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
        configure: C,
    ) -> Result<Response, ApiError>
    where
        C: Fn(&mut Request),
    {
        self.execute_body_with(
            method,
            path,
            extra_headers,
            body.map(RequestBody::Json),
            configure,
        )
        .await
    }

    async fn execute_body_with<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<RequestBody>,
        configure: C,
    ) -> Result<Response, ApiError>
    where
        C: Fn(&mut Request),
    {
        let mut request = self.make_request(&method, path, &extra_headers, body);
        configure(&mut request);
        let request = prepare_request(request).await?;
        let make_request = || request.clone();

        let response = run_with_request_telemetry(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    transport.execute(req).await
                }
            },
        )
        .await?;

        Ok(response)
    }

    #[instrument(
        name = "endpoint_session.stream_encoded_json_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn stream_encoded_json_with<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<EncodedJsonBody>,
        configure: C,
    ) -> Result<StreamResponse, ApiError>
    where
        C: Fn(&mut Request),
    {
        let body = body.map(RequestBody::EncodedJson);
        let mut request = self.make_request(&method, path, &extra_headers, body);
        configure(&mut request);
        let request = prepare_request(request).await?;
        let make_request = || request.clone();
        let header_timeout = self.provider.stream_idle_timeout;
        let header_deadline_elapsed = AtomicBool::new(false);

        // The new header deadline must not replay an ambiguously dispatched
        // model request. Concrete transport errors retain their existing policy.
        // Scope the deadline to obtaining headers, not the response body's lifetime.
        let stream = run_with_request_telemetry_non_idempotent(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                let header_deadline_elapsed = &header_deadline_elapsed;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    // Authentication and safe pre-dispatch retry backoff are
                    // not response silence. Give each dispatch its own budget.
                    tokio::time::timeout(header_timeout, transport.stream(req))
                        .await
                        .unwrap_or_else(|_| {
                            header_deadline_elapsed.store(true, Ordering::Relaxed);
                            Err(TransportError::Timeout)
                        })
                }
            },
        )
        .await;
        if header_deadline_elapsed.load(Ordering::Relaxed) {
            return Err(ApiError::ProviderFailure {
                code: Some("response_header_timeout".to_string()),
                message: format!(
                    "deadline waiting for model response headers after {}ms",
                    header_timeout.as_millis()
                ),
            });
        }

        Ok(stream?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn compressed_request_preparation_runs_on_blocking_pool() {
        let runtime_thread = std::thread::current().id();
        let preparation_thread = Arc::new(Mutex::new(None));
        let observed_thread = preparation_thread.clone();
        let request = Request::new(Method::POST, "https://example.com/responses".to_string())
            .with_json(&json!({"model": "test-model"}))
            .with_compression(RequestCompression::Zstd);

        let prepared = prepare_request_with(request, move |request| {
            *observed_thread
                .lock()
                .expect("preparation thread mutex should not be poisoned") =
                Some(std::thread::current().id());
            request.into_prepared()
        })
        .await
        .expect("request should prepare");

        let preparation_thread = preparation_thread
            .lock()
            .expect("preparation thread mutex should not be poisoned")
            .expect("preparation thread should be recorded");
        assert_ne!(preparation_thread, runtime_thread);
        assert_eq!(prepared.compression, RequestCompression::None);
    }
    #[tokio::test(start_paused = true)]
    #[ignore = "local response-header deadline benchmark"]
    async fn latency_edge_benchmark() {
        struct PendingHeaders;
        impl HttpTransport for PendingHeaders {
            async fn execute(&self, _: Request) -> Result<Response, TransportError> {
                unreachable!("stream only")
            }
            async fn stream(&self, _: Request) -> Result<StreamResponse, TransportError> {
                std::future::pending().await
            }
        }
        struct NoAuth;
        impl crate::auth::AuthProvider for NoAuth {
            fn add_auth_headers(&self, _: &mut HeaderMap) {}
        }
        let session = EndpointSession::new(PendingHeaders, Provider {
            name: "local probe".into(), base_url: "http://127.0.0.1".into(),
            query_params: None, headers: HeaderMap::new(),
            retry: crate::provider::RetryConfig {
                max_retries: 0, base_delay: std::time::Duration::ZERO,
                retry_429: false, retry_5xx: false, retry_transport: false,
            },
            stream_idle_timeout: std::time::Duration::from_millis(50),
        }, Arc::new(NoAuth));
        let result = tokio::time::timeout(std::time::Duration::from_millis(75),
            session.stream_encoded_json_with(Method::POST, "responses", HeaderMap::new(), None, |_| {})).await;
        let bounded = matches!(result, Ok(Err(ApiError::ProviderFailure { code, .. }))
            if code.as_deref() == Some("response_header_timeout"));
        let result = format!("latency_edge headers: configured_budget_ms=50 externally_stopped_ms=75 deadline_applied={bounded}\n");
        eprint!("{result}");
        if let Some(path) = std::env::var_os("KD4_TRANSPORT_EDGE_OUTPUT") {
            use std::io::Write;
            std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap()
                .write_all(result.as_bytes()).unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn header_deadline_excludes_authentication_and_safe_retry_backoff() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        struct DelayedAuth(AtomicUsize);
        impl crate::auth::AuthProvider for DelayedAuth {
            fn add_auth_headers(&self, _: &mut HeaderMap) {}

            fn apply_auth(&self, request: Request) -> crate::auth::AuthProviderFuture<'_> {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(crate::auth::AuthError::Transient("credentials unavailable".into()));
                    }
                    Ok(request)
                })
            }
        }
        struct ImmediateHeaders(Arc<AtomicUsize>);
        impl HttpTransport for ImmediateHeaders {
            async fn execute(&self, _: Request) -> Result<Response, TransportError> {
                unreachable!("stream only")
            }

            async fn stream(&self, _: Request) -> Result<StreamResponse, TransportError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(StreamResponse {
                    status: http::StatusCode::OK,
                    headers: HeaderMap::new(),
                    bytes: Box::pin(futures::stream::empty()),
                })
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let auth = Arc::new(DelayedAuth(AtomicUsize::new(0)));
        let session = EndpointSession::new(ImmediateHeaders(calls.clone()), Provider {
            name: "auth retry probe".into(), base_url: "http://127.0.0.1".into(),
            query_params: None, headers: HeaderMap::new(),
            retry: crate::provider::RetryConfig {
                max_retries: 1, base_delay: Duration::from_millis(100),
                retry_429: false, retry_5xx: false, retry_transport: true,
            },
            stream_idle_timeout: Duration::from_millis(50),
        }, auth.clone());
        let result = session.stream_encoded_json_with(
            Method::POST, "responses", HeaderMap::new(), None, |_| {},
        ).await;
        assert!(result.is_ok(), "authentication/backoff are not response silence");
        assert_eq!(auth.0.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn header_deadline_does_not_replay_or_limit_the_response_body() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        struct HeaderProbe {
            delay: Option<Duration>,
            calls: Arc<AtomicUsize>,
        }
        impl HttpTransport for HeaderProbe {
            async fn execute(&self, _: Request) -> Result<Response, TransportError> {
                unreachable!("stream only")
            }

            async fn stream(&self, _: Request) -> Result<StreamResponse, TransportError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let Some(delay) = self.delay else {
                    return std::future::pending().await;
                };
                if delay.is_zero() {
                    return Err(TransportError::Timeout);
                }
                tokio::time::sleep(delay).await;
                Ok(StreamResponse {
                    status: http::StatusCode::OK,
                    headers: HeaderMap::new(),
                    bytes: Box::pin(futures::stream::once(async {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        Ok(bytes::Bytes::from_static(b"body"))
                    })),
                })
            }
        }
        struct NoAuth;
        impl crate::auth::AuthProvider for NoAuth {
            fn add_auth_headers(&self, _: &mut HeaderMap) {}
        }

        for delay in [None, Some(Duration::ZERO), Some(Duration::from_millis(10))] {
            let calls = Arc::new(AtomicUsize::new(0));
            let session = EndpointSession::new(
                HeaderProbe {
                    delay,
                    calls: calls.clone(),
                },
                Provider {
                    name: "local probe".into(),
                    base_url: "http://127.0.0.1".into(),
                    query_params: None,
                    headers: HeaderMap::new(),
                    retry: crate::provider::RetryConfig {
                        max_retries: 3,
                        base_delay: Duration::ZERO,
                        retry_429: false,
                        retry_5xx: false,
                        retry_transport: true,
                    },
                    stream_idle_timeout: Duration::from_millis(50),
                },
                Arc::new(NoAuth),
            );
            let started = tokio::time::Instant::now();
            let result = session
                .stream_encoded_json_with(Method::POST, "responses", HeaderMap::new(), None, |_| {})
                .await;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "ambiguous sends must not replay"
            );
            if delay.is_none() {
                let error = result.err().expect("header deadline");
                assert!(matches!(&error, ApiError::ProviderFailure { code, .. }
                    if code.as_deref() == Some("response_header_timeout")));
                assert!(!crate::api_bridge::map_api_error(error).is_retryable());
                assert_eq!(started.elapsed(), Duration::from_millis(50));
            } else if delay == Some(Duration::ZERO) {
                let error = result.err().expect("concrete transport timeout");
                assert!(matches!(&error, ApiError::Transport(TransportError::Timeout)));
                assert!(crate::api_bridge::map_api_error(error).is_retryable());
                assert_eq!(started.elapsed(), Duration::ZERO);
            } else {
                assert_eq!(started.elapsed(), Duration::from_millis(10));
                let mut response = result.unwrap();
                let body = futures::StreamExt::next(&mut response.bytes)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(body.as_ref(), b"body");
                assert_eq!(started.elapsed(), Duration::from_millis(110));
            }
        }
    }
}
