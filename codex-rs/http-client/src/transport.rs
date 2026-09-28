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
use std::time::Duration;
use tracing::Level;
use tracing::enabled;
use tracing::trace;

pub type ByteStream = BoxStream<'static, Result<Bytes, TransportError>>;

const ERROR_BODY_READ_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

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
        if err.is_connect() {
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

async fn read_error_body(response: reqwest::Response) -> Option<String> {
    // Status and Retry-After are already authoritative. Diagnostics must not
    // hold an otherwise actionable failure hostage to a stalled/oversized body.
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    let read = tokio::time::timeout(ERROR_BODY_READ_TIMEOUT, async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.ok()?;
            let remaining = MAX_ERROR_BODY_BYTES - bytes.len();
            bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            if bytes.len() == MAX_ERROR_BODY_BYTES {
                return Some(false);
            }
        }
        Some(true)
    })
    .await;
    match read {
        Ok(None) => None,
        Err(_) if bytes.is_empty() => None,
        result => {
            let mut body = String::from_utf8_lossy(&bytes).into_owned();
            if !matches!(result, Ok(Some(true))) {
                body.push_str("\n[HTTP error body truncated]");
            }
            Some(body)
        }
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
            let body = read_error_body(resp).await;
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
            let body = read_error_body(resp).await;
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
