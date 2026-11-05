//! Probe evidence keeps wire values available to callers, never in Debug or Display.

use crate::error::ApiError;
use codex_client::TransportError;
use http::HeaderMap;
use http::StatusCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsesWebsocketProbeFailureStage {
    Tls,
    Unknown,
}

/// A probe failure, including upgrade evidence observed before a later read failed.
#[derive(thiserror::Error)]
#[error("Responses WebSocket probe failed")]
pub struct ResponsesWebsocketProbeError {
    #[source]
    pub error: Box<ApiError>,
    pub stage: ResponsesWebsocketProbeFailureStage,
    pub status: Option<StatusCode>,
    /// Only allowlisted tracking headers; values still require caller redaction.
    pub headers: Option<HeaderMap>,
}

impl ResponsesWebsocketProbeError {
    pub(super) fn before_upgrade(error: ApiError, stage: ResponsesWebsocketProbeFailureStage) -> Self {
        let (status, headers) = match &error {
            ApiError::Transport(TransportError::Http {
                status, headers, ..
            }) => (Some(*status), headers.as_ref().map(probe_tracking_headers)),
            _ => (None, None),
        };
        Self {
            error: Box::new(error),
            stage,
            status,
            headers,
        }
    }
}

impl From<ApiError> for ResponsesWebsocketProbeError {
    fn from(error: ApiError) -> Self {
        Self::before_upgrade(error, ResponsesWebsocketProbeFailureStage::Unknown)
    }
}

impl std::fmt::Debug for ResponsesWebsocketProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponsesWebsocketProbeError")
            .field("stage", &self.stage)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

pub(super) fn probe_tracking_headers(source: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in ["x-request-id", "x-oai-request-id", "cf-ray", "x-ms-request-id"] {
        for value in source.get_all(name) {
            headers.append(name, value.clone());
        }
    }
    headers
}
