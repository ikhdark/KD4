//! Errors returned by the shared Codex HTTP transport.

use crate::RetryAfter;
use crate::client::HttpError;
use http::HeaderMap;
use http::StatusCode;
use thiserror::Error;

/// A connector error together with its scope in the original request.
#[derive(Debug, Error)]
#[error("{source}")]
pub struct ConnectionError {
    #[source]
    source: HttpError,
    initial_request_not_sent: bool,
}

impl ConnectionError {
    pub(crate) fn new(source: HttpError, initial_request_not_sent: bool) -> Self {
        Self {
            source,
            initial_request_not_sent,
        }
    }

    /// True only if the transport can prove that the original request was not sent.
    pub fn initial_request_not_sent(&self) -> bool {
        self.initial_request_not_sent
    }
}

impl std::ops::Deref for ConnectionError {
    type Target = HttpError;

    fn deref(&self) -> &Self::Target {
        &self.source
    }
}

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
    /// The connection deadline elapsed before any model request was sent.
    #[error("connection establishment timed out")]
    ConnectionTimeout,
    #[error("connection failed: {0}")]
    Connection(#[source] ConnectionError),
    #[error("network error: {0}")]
    Network(String),
    /// A transient failure proven to have happened before the request was
    /// dispatched to the transport. Retrying this error cannot duplicate a
    /// non-idempotent request.
    #[error("pre-dispatch error: {0}")]
    PreDispatch(String),
    #[error("request build error: {0}")]
    Build(String),
    /// The server accepted the request; repeating it is not size recovery.
    #[error("successful response exceeded the {limit} byte receive limit")]
    ResponseTooLarge { limit: usize },
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
