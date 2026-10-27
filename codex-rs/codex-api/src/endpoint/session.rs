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
        max_response_bytes: usize,
    ) -> Result<Response, ApiError> {
        self.execute_body_with(
            method,
            path,
            extra_headers,
            body.map(RequestBody::EncodedJson),
            |req| req.max_response_bytes = Some(max_response_bytes),
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
        max_response_bytes: usize,
    ) -> Result<Response, ApiError> {
        let body = body.map(RequestBody::EncodedJson);
        let mut request = self.make_request(&method, path, &extra_headers, body);
        request.max_response_bytes = Some(max_response_bytes);
        let request = prepare_request(request).await?;
        let make_request = || request.clone();

        let response = run_with_request_telemetry_non_idempotent(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req, _attempt| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    codex_client::with_dispatch_guard(auth.dispatch_guard(), transport.execute(req))
                        .await
                }
            },
        )
        .await;

        match response {
            Err(error @ (TransportError::Timeout | TransportError::Network(_))) => {
                Err(ApiError::ProviderFailure {
                    code: Some("dispatch_outcome_unknown".into()),
                    message: format!("Request outcome is unknown after dispatch ({error}); automatic replay was stopped."),
                })
            }
            result => Ok(result?),
        }
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
                    codex_client::with_dispatch_guard(auth.dispatch_guard(), transport.execute(req))
                        .await
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
        // model request. Native post-send errors need the same outer guard.
        // Scope the deadline to obtaining headers, not the response body's lifetime.
        let stream = run_with_request_telemetry_non_idempotent(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req, attempt| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                let header_deadline_elapsed = &header_deadline_elapsed;
                let request_telemetry = self.request_telemetry.clone();
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    if let Some(telemetry) = request_telemetry {
                        telemetry.on_request_dispatched_attempt(attempt);
                    }
                    // Authentication and safe pre-dispatch retry backoff are
                    // not response silence. Give each dispatch its own budget.
                    codex_client::with_dispatch_guard(
                        auth.dispatch_guard(),
                        transport.stream_with_header_timeout(req, header_timeout),
                    )
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

        match stream {
            Err(error @ (TransportError::Timeout | TransportError::Network(_))) => {
                Err(ApiError::ProviderFailure {
                    code: Some("dispatch_outcome_unknown".to_string()),
                    message: format!(
                        "Model request outcome is unknown after dispatch ({error}); automatic replay was stopped to avoid duplicate generation."
                    ),
                })
            }
            result => Ok(result?),
        }
    }
}
