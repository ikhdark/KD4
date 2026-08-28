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
        body: Option<&RequestBody>,
    ) -> Request {
        let mut req = self.provider.build_request(method.clone(), path);
        req.headers.extend(extra_headers.clone());
        if let Some(body) = body {
            req.body = Some(body.clone());
        }
        req
    }

    pub(crate) async fn execute(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
    ) -> Result<Response, ApiError> {
        self.execute_with(method, path, extra_headers, body, |_| {})
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
        body: Option<Value>,
    ) -> Result<Response, ApiError> {
        let body = body.map(RequestBody::Json);
        let request = self.make_request(&method, path, &extra_headers, body.as_ref());
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
        let body = body.map(RequestBody::Json);
        let mut request = self.make_request(&method, path, &extra_headers, body.as_ref());
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
        let mut request = self.make_request(&method, path, &extra_headers, body.as_ref());
        configure(&mut request);
        let request = prepare_request(request).await?;
        let make_request = || request.clone();

        let stream = run_with_request_telemetry_non_idempotent(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    transport.stream(req).await
                }
            },
        )
        .await?;

        Ok(stream)
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
}
