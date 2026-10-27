use crate::RetryAfter;
use crate::client::HttpClient;
use crate::client::RequestBuilder;
use crate::error::TransportError;
use crate::request::Request;
use crate::request::RequestBody;
use crate::request::Response;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use http::HeaderMap;
use http::Method;
use http::StatusCode;
use http::header::IF_NONE_MATCH;
use tracing::Level;
use tracing::enabled;
use tracing::trace;

pub type ByteStream = BoxStream<'static, Result<Bytes, TransportError>>;

const ERROR_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

tokio::task_local! {
    static TRANSPORT_TIMING: std::cell::Cell<TransportTiming>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TransportTiming {
    /// Concrete reqwest handoff, not proof of a socket write or provider receipt.
    pub send_started_at: Option<std::time::Instant>,
    pub response_headers_at: Option<std::time::Instant>,
    /// First nonempty raw response-body chunk, before SSE framing/decoding.
    pub first_response_body_byte_at: Option<std::time::Instant>,
    pub response_headers: Option<std::time::Duration>,
    pub proxy_resolution: Option<std::time::Duration>,
    pub client_pool_selection: Option<std::time::Duration>,
}

/// Captures send-to-headers latency from the concrete transport, excluding auth,
/// request encoding and response body reads. This cumulative interval includes
/// route selection, redirects, connection setup and upload; it is not a DNS, TCP or TLS measurement.
/// The task-local scope isolates concurrent requests and is dropped on cancellation.
pub async fn capture_transport_timing<F: std::future::Future>(
    future: F,
) -> (F::Output, TransportTiming) {
    TRANSPORT_TIMING
        .scope(
            std::cell::Cell::new(TransportTiming::default()),
            async move {
                let result = future.await;
                (result, TRANSPORT_TIMING.with(std::cell::Cell::get))
            },
        )
        .await
}

fn record_response_header_time(duration: std::time::Duration) {
    record_timing(|timing| timing.response_headers = Some(duration));
    tracing::debug!(
        event.name = "codex.http.send_to_response_headers",
        duration_us = duration.as_micros() as u64,
        provenance = "transport_send_to_response_headers",
    );
}

pub(crate) fn record_timing(update: impl FnOnce(&mut TransportTiming)) {
    let _ = TRANSPORT_TIMING.try_with(|slot| {
        let mut timing = slot.get();
        update(&mut timing);
        slot.set(timing);
    });
}

pub(crate) fn record_send_started() {
    record_timing(|timing| {
        timing
            .send_started_at
            .get_or_insert_with(std::time::Instant::now);
    });
}

pub(crate) fn record_headers_received() {
    record_timing(|timing| {
        timing
            .response_headers_at
            .get_or_insert_with(std::time::Instant::now);
    });
}

/// Same-task scalar observations only: no locks, atomics, logging, or IO.
pub fn record_first_response_body_byte() {
    record_timing(|timing| {
        timing
            .first_response_body_byte_at
            .get_or_insert_with(std::time::Instant::now);
    });
}

pub fn current_transport_timing() -> TransportTiming {
    TRANSPORT_TIMING
        .try_with(std::cell::Cell::get)
        .unwrap_or_default()
}

/// Reuses the request-local timing slot in a spawned stream owner. This wraps
/// the already-polled future without spawning work or adding an async wait.
pub fn with_transport_timing<F: std::future::Future>(
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    TRANSPORT_TIMING.scope(
        std::cell::Cell::new(TransportTiming::default()),
        future,
    )
}

/// Collect a bounded diagnostic prefix under one deadline. Failure headers
/// remain authoritative when body delivery stalls or exceeds the byte limit.
pub async fn collect_error_body(
    stream: impl futures::Stream<Item = Result<Bytes, reqwest::Error>>,
) -> Option<String> {
    tokio::pin!(stream);
    let deadline = tokio::time::Instant::now() + ERROR_BODY_TIMEOUT;
    let mut body = Vec::new();
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                let remaining = MAX_ERROR_BODY_BYTES - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if body.len() == MAX_ERROR_BODY_BYTES {
                    break;
                }
            }
            Ok(None) => return Some(String::from_utf8_lossy(&body).into_owned()),
            // Preserve the existing unavailable-body contract on transport errors.
            Ok(Some(Err(_))) => return None,
            Err(_) => break,
        }
    }
    Some(format!(
        "{}\n[HTTP error body incomplete: diagnostic limit reached]",
        String::from_utf8_lossy(&body)
    ))
}

pub struct StreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: ByteStream,
}

async fn collect_success_body(
    stream: impl futures::Stream<Item = Result<Bytes, reqwest::Error>>,
    limit: usize,
) -> Result<Bytes, TransportError> {
    collect_response_body(stream, limit).await.map(Bytes::from)
}

/// Collect a decoded response without retaining more than `limit` body bytes.
/// On overflow the remaining stream is dropped, never drained or retried.
pub async fn collect_response_body(
    stream: impl futures::Stream<Item = Result<Bytes, reqwest::Error>>,
    limit: usize,
) -> Result<Vec<u8>, TransportError> {
    tokio::pin!(stream);
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ReqwestTransport::map_error)?;
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(TransportError::ResponseTooLarge { limit });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub trait HttpTransport: Send + Sync {
    fn execute(
        &self,
        req: Request,
    ) -> impl std::future::Future<Output = Result<Response, TransportError>> + Send;
    fn stream(
        &self,
        req: Request,
    ) -> impl std::future::Future<Output = Result<StreamResponse, TransportError>> + Send;

    /// The outer result reports expiry before headers; implementations that
    /// collect error diagnostics must stop that deadline at header receipt.
    fn stream_with_header_timeout(
        &self,
        req: Request,
        timeout: std::time::Duration,
    ) -> impl std::future::Future<Output = Result<Result<StreamResponse, TransportError>, tokio::time::error::Elapsed>> + Send {
        tokio::time::timeout(timeout, self.stream(req))
    }
}

#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: TransportClient,
}

#[derive(Clone, Debug)]
enum TransportClient {
    Fixed(HttpClient),
    Routed(Box<crate::RouteAwareClientPool>),
}

impl ReqwestTransport {
    async fn finish_stream_response(
        url: String,
        resp: crate::HttpResponse,
    ) -> Result<StreamResponse, TransportError> {
        let status = resp.status();
        let headers = resp.headers().clone();
        if !status.is_success() {
            let retry_after = RetryAfter::from_headers(&headers);
            let body = collect_error_body(resp.bytes_stream()).await;
            return Err(TransportError::Http {
                status,
                url: Some(url),
                headers: Some(headers),
                body,
                retry_after,
            });
        }
        let stream = resp
            .bytes_stream()
            .map(|result| result.map_err(Self::map_error));
        Ok(StreamResponse {
            status,
            headers,
            bytes: Box::pin(stream),
        })
    }

    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client: TransportClient::Fixed(HttpClient::new(client)),
        }
    }

    pub fn from_http_client(client: HttpClient) -> Self {
        Self {
            client: TransportClient::Fixed(client),
        }
    }

    /// Keeps route selection at dispatch, including each redirect hop.
    pub fn from_client_pool(pool: crate::RouteAwareClientPool) -> Self {
        Self {
            client: TransportClient::Routed(Box::new(pool)),
        }
    }

    fn build(client: &HttpClient, req: Request) -> Result<RequestBuilder, TransportError> {
        let prepared = req.prepare_body_for_send().map_err(TransportError::Build)?;

        let Request {
            method,
            url,
            headers: _,
            body: _,
            compression: _,
            timeout,
            max_response_bytes: _,
        } = req;

        let mut builder = client.request(
            Method::from_bytes(method.as_str().as_bytes()).unwrap_or(Method::GET),
            &url,
        );

        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }

        builder = builder.headers(prepared.headers);
        if let Some(body) = prepared.body {
            builder = builder.body(body);
        }
        Ok(builder)
    }

    async fn send(&self, req: Request) -> Result<crate::HttpResponse, TransportError> {
        match &self.client {
            TransportClient::Fixed(client) => {
                let builder = Self::build(client, req)?;
                let start = std::time::Instant::now();
                let response = crate::admit_dispatch(builder.send())
                    .await
                    .map_err(TransportError::Build)?
                    .map_err(Self::map_error)?;
                record_response_header_time(start.elapsed());
                Ok(response)
            }
            TransportClient::Routed(pool) => {
                let prepared = req.prepare_body_for_send().map_err(TransportError::Build)?;
                let mut builder = pool.request(req.method, req.url).headers(prepared.headers);
                if let Some(timeout) = req.timeout {
                    builder = builder.timeout(timeout);
                }
                if let Some(body) = prepared.body {
                    builder = builder.body(body);
                }
                // Includes route selection and redirect hops, but not body reads or auth.
                let start = std::time::Instant::now();
                let response = builder.send().await.map_err(|error| match error {
                    crate::RouteAwareRequestError::Request(error) => Self::map_error(error),
                    crate::RouteAwareRequestError::Timeout => TransportError::Timeout,
                    error => TransportError::Build(format!(
                        "HTTP routing failed: {}",
                        error.safe_category()
                    )),
                })?;
                record_response_header_time(start.elapsed());
                Ok(response)
            }
        }
    }

    fn map_error(err: reqwest::Error) -> TransportError {
        if err.is_builder() || is_permanent_connection_error(&err) {
            TransportError::Build(format!(
                "permanent transport configuration failure: {:#}",
                err.without_url()
            ))
        } else if err.is_connect() {
            TransportError::Connection(err.without_url())
        } else if err.is_timeout() {
            TransportError::Timeout
        } else {
            TransportError::Network(err.without_url().to_string())
        }
    }

    fn trace_request(&self, req: &Request) {
        if let TransportClient::Fixed(client) = &self.client
            && client.request_logging_enabled()
            && enabled!(Level::TRACE)
        {
            trace!(
                "{} to {}: {}",
                req.method,
                req.url,
                request_body_for_trace(req)
            );
        }
    }
}

/// Recognize configuration and certificate failures without treating transient
/// socket errors or remote TLS alerts as permanent network failures.
pub fn is_permanent_connection_error(mut error: &(dyn std::error::Error + 'static)) -> bool {
    loop {
        if error.downcast_ref::<rustls::Error>().is_some_and(|error| {
            matches!(
                error,
                rustls::Error::InvalidCertificate(_)
                    | rustls::Error::NoCertificatesPresented
                    | rustls::Error::UnsupportedNameType
                    | rustls::Error::PeerIncompatible(_)
                    | rustls::Error::InvalidCertRevocationList(_)
                    | rustls::Error::InconsistentKeys(_)
                    | rustls::Error::BadMaxFragmentSize
                    | rustls::Error::NoApplicationProtocol
            )
        }) || error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::InvalidInput)
        {
            return true;
        }
        // io::Error::source can delegate to the wrapped error's source, skipping
        // the typed TLS error itself. Inspect its payload before walking on.
        let source = error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            .map(|inner| inner as &(dyn std::error::Error + 'static))
            .or_else(|| error.source());
        let Some(source) = source else { return false };
        error = source;
    }
}

fn request_body_for_trace(req: &Request) -> String {
    match req.body.as_ref() {
        Some(RequestBody::Json(body)) => format!("<JSON body: {} bytes>", body.to_string().len()),
        Some(RequestBody::EncodedJson(body)) => {
            format!("<encoded JSON body: {} bytes>", body.as_bytes().len())
        }
        Some(RequestBody::Raw(body)) => format!("<raw body: {} bytes>", body.len()),
        Some(RequestBody::InvalidJson(_)) => "<invalid JSON body>".to_string(),
        None => String::new(),
    }
}

impl HttpTransport for ReqwestTransport {
    async fn execute(&self, req: Request) -> Result<Response, TransportError> {
        self.trace_request(&req);

        let accepts_not_modified = req.headers.contains_key(IF_NONE_MATCH);
        let max_response_bytes = req.max_response_bytes;
        let url = req.url.clone();
        let resp = self.send(req).await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        if !(status.is_success() || status == StatusCode::NOT_MODIFIED && accepts_not_modified) {
            let retry_after = RetryAfter::from_headers(&headers);
            let body = collect_error_body(resp.bytes_stream()).await;
            return Err(TransportError::Http {
                status,
                url: Some(url),
                headers: Some(headers),
                body,
                retry_after,
            });
        }
        let bytes = if let Some(limit) = max_response_bytes {
            if resp
                .content_length()
                .is_some_and(|length| length > limit as u64)
            {
                return Err(TransportError::ResponseTooLarge { limit });
            }
            collect_success_body(resp.bytes_stream(), limit).await?
        } else {
            resp.bytes().await.map_err(Self::map_error)?
        };
        Ok(Response {
            status,
            headers,
            body: bytes,
        })
    }

    async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError> {
        self.trace_request(&req);

        let url = req.url.clone();
        let resp = self.send(req).await?;
        Self::finish_stream_response(url, resp).await
    }

    async fn stream_with_header_timeout(
        &self,
        req: Request,
        timeout: std::time::Duration,
    ) -> Result<Result<StreamResponse, TransportError>, tokio::time::error::Elapsed> {
        self.trace_request(&req);
        let url = req.url.clone();
        // Route selection, redirects and dispatch are bounded until headers.
        // Once received, status/advice survive the independent diagnostic wait.
        let response = tokio::time::timeout(timeout, self.send(req)).await?;
        Ok(match response {
            Ok(response) => Self::finish_stream_response(url, response).await,
            Err(error) => Err(error),
        })
    }
}

