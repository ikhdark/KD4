//! Errors returned by the shared Codex HTTP transport.

use crate::RetryAfter;
use crate::client::HttpError;
use http::HeaderMap;
use http::StatusCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("http {status}: {body:?}")]
    Http {
        status: StatusCode,
        url: Option<String>,
        headers: Option<HeaderMap>,
        body: Option<String>,
        retry_after: Option<RetryAfter>,
    },
    #[error("retry limit reached")]
    RetryLimit,
    #[error("timeout")]
    Timeout,
    #[error("connection failed: {0}")]
    Connection(#[source] HttpError),
    #[error("network error: {0}")]
    Network(String),
    /// A transient failure proven to have happened before the request was
    /// dispatched to the transport. Retrying this error cannot duplicate a
    /// non-idempotent request.
    #[error("pre-dispatch error: {0}")]
    PreDispatch(String),
    #[error("request build error: {0}")]
    Build(String),
}

impl TransportError {
    /// Returns advice captured at response receipt, without restarting its clock.
    pub fn retry_after(&self) -> Option<RetryAfter> {
        match self {
            Self::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum StreamError {
    #[error("stream failed: {0}")]
    Stream(String),
    #[error("timeout")]
    Timeout,
}
