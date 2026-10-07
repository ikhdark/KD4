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

// Failure headers are authoritative even when diagnostic body delivery stalls.
// Use one deadline, not a fresh timeout for every chunk of an endless body.
async fn collect_error_body(
    stream: impl futures::Stream<Item = Result<Bytes, reqwest::Error>>,
) -> Option<String> {
    tokio::pin!(stream);
    let deadline = tokio::time::Instant::now() + ERROR_BODY_TIMEOUT;
    let mut body = Vec::new();
    loop {
        if tokio::time::Instant::now() >= deadline { break; }
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

pub trait HttpTransport: Send + Sync {
    fn execute(
        &self,
        req: Request,
    ) -> impl std::future::Future<Output = Result<Response, TransportError>> + Send;
    fn stream(
        &self,
        req: Request,
    ) -> impl std::future::Future<Output = Result<StreamResponse, TransportError>> + Send;
}

#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: HttpClient,
}

impl ReqwestTransport {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client: HttpClient::new(client),
        }
    }

    pub fn from_http_client(client: HttpClient) -> Self {
        Self { client }
    }

    fn build(&self, req: Request) -> Result<RequestBuilder, TransportError> {
        let prepared = req.prepare_body_for_send().map_err(TransportError::Build)?;

        let Request {
            method,
            url,
            headers: _,
            body: _,
            compression: _,
            timeout,
        } = req;

        let mut builder = self.client.request(
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

    fn map_error(err: reqwest::Error) -> TransportError {
        if err.is_builder() || is_permanent_connection_error(&err) {
            TransportError::Build(format!("permanent transport configuration failure: {:#}", err.without_url()))
        } else if err.is_connect() {
            TransportError::Connection(err.without_url())
        } else if err.is_timeout() {
            TransportError::Timeout
        } else {
            TransportError::Network(err.without_url().to_string())
        }
    }

    fn trace_request(&self, req: &Request) {
        if self.client.request_logging_enabled() && enabled!(Level::TRACE) {
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
        if error.downcast_ref::<rustls::Error>().is_some_and(|error| matches!(error,
            rustls::Error::InvalidCertificate(_)
                | rustls::Error::NoCertificatesPresented
                | rustls::Error::UnsupportedNameType
                | rustls::Error::PeerIncompatible(_)
                | rustls::Error::InvalidCertRevocationList(_)
                | rustls::Error::InconsistentKeys(_)
                | rustls::Error::BadMaxFragmentSize
                | rustls::Error::NoApplicationProtocol
        ))
            || error.downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::InvalidInput)
        {
            return true;
        }
        // io::Error::source can delegate to the wrapped error's source, skipping
        // the typed TLS error itself. Inspect its payload before walking on.
        let source = error.downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            .map(|inner| inner as &(dyn std::error::Error + 'static))
            .or_else(|| error.source());
        let Some(source) = source else { return false };
        error = source;
    }
}

fn request_body_for_trace(req: &Request) -> String {
    match req.body.as_ref() {
        Some(RequestBody::Json(body)) => body.to_string(),
        Some(RequestBody::EncodedJson(body)) => {
            String::from_utf8_lossy(body.trace_bytes()).into_owned()
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
        let url = req.url.clone();
        let builder = self.build(req)?;
        let resp = builder.send().await.map_err(Self::map_error)?;
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
        let bytes = resp.bytes().await.map_err(Self::map_error)?;
        Ok(Response {
            status,
            headers,
            body: bytes,
        })
    }

    async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError> {
        self.trace_request(&req);

        let url = req.url.clone();
        let builder = self.build(req)?;
        let resp = builder.send().await.map_err(Self::map_error)?;
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
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
