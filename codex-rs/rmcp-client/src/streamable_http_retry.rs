use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use codex_exec_server::ExecServerError;
use rmcp::service::RoleClient;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpError;
use tokio::time;
use tokio::time::Instant;
use tracing::warn;

use crate::elicitation_client_service::ElicitationClientService;
use crate::http_client_adapter::StreamableHttpClientAdapterError;
use crate::http_client_adapter::is_retryable_http_status;
use crate::oauth::OAuthPersistor;

use super::InitializeBudget;
use super::PendingTransport;
use super::RmcpClient;

const JSON_RPC_INTERNAL_ERROR_CODE: i64 = -32603;
pub(super) const STREAMABLE_HTTP_RETRY_DELAYS_MS: [u64; 2] = [250, 1_000];

impl RmcpClient {
    pub(super) async fn connect_pending_transport_with_initialize_retries(
        &self,
        initial_transport: PendingTransport,
        client_service: ElicitationClientService,
        budget: Option<InitializeBudget>,
    ) -> Result<(
        Arc<RunningService<RoleClient, ElicitationClientService>>,
        Option<OAuthPersistor>,
    )> {
        let should_retry = match &initial_transport {
            PendingTransport::Stdio { .. } => false,
            PendingTransport::StreamableHttp { .. }
            | PendingTransport::StreamableHttpWithOAuth { .. } => true,
        };
        let mut pending_transport = Some(initial_transport);

        for (attempt, retry_delay_ms) in STREAMABLE_HTTP_RETRY_DELAYS_MS
            .iter()
            .copied()
            .map(Some)
            .chain(std::iter::once(None))
            .enumerate()
        {
            let transport = match pending_transport.take() {
                Some(transport) => transport,
                None => {
                    InitializeBudget::observe(
                        budget,
                        Self::create_pending_transport(&self.transport_recipe),
                    )
                    .await??
                }
            };
            match Self::connect_pending_transport(transport, client_service.clone(), budget).await
            {
                Ok(result) => return Ok(result),
                Err(error) if should_retry && Self::is_retryable_initialize_error(&error) => {
                    let Some(retry_delay_ms) = retry_delay_ms else {
                        return Err(error);
                    };
                    let delay = retry_delay(error.as_ref(), retry_delay_ms);
                    warn!(
                        attempt = attempt + 1,
                        max_attempts = STREAMABLE_HTTP_RETRY_DELAYS_MS.len() + 1,
                        delay_ms = delay.as_millis(),
                        error = %error,
                        "streamable HTTP MCP initialize failed with a retryable error; retrying"
                    );
                    if let Some(budget) = budget {
                        if !sleep_with_retry_deadline(delay, Some(budget.deadline)).await {
                            return Err(budget.timeout_error());
                        }
                    } else {
                        sleep_with_retry_deadline(delay, None).await;
                    }
                }
                Err(error) => return Err(error),
            }
        }

        unreachable!("initialize retry loop should return on success or final error")
    }

    fn is_retryable_initialize_error(error: &anyhow::Error) -> bool {
        error.chain().any(|source| {
            source
                .downcast_ref::<HandshakeError>()
                .is_some_and(|error| Self::is_retryable_client_initialize_error(&error.source))
                || source
                    .downcast_ref::<rmcp::service::ClientInitializeError>()
                    .is_some_and(Self::is_retryable_client_initialize_error)
        })
    }

    fn is_retryable_client_initialize_error(error: &rmcp::service::ClientInitializeError) -> bool {
        match error {
            rmcp::service::ClientInitializeError::TransportError { error, context }
                if context.as_ref() == "send initialize request" =>
            {
                error
                    .error
                    .downcast_ref::<StreamableHttpError<StreamableHttpClientAdapterError>>()
                    .is_some_and(Self::is_retryable_streamable_http_error)
            }
            rmcp::service::ClientInitializeError::TransportError { error, context }
                if context.as_ref() == "send initialized notification" =>
            {
                error
                    .error
                    .downcast_ref::<StreamableHttpError<StreamableHttpClientAdapterError>>()
                    .is_some_and(|error| {
                        matches!(error, StreamableHttpError::TransportChannelClosed)
                            || Self::is_retryable_streamable_http_error(error)
                    })
            }
            _ => false,
        }
    }

    pub(super) fn is_retryable_streamable_http_error(
        error: &StreamableHttpError<StreamableHttpClientAdapterError>,
    ) -> bool {
        match error {
            StreamableHttpError::Client(StreamableHttpClientAdapterError::HttpRequest(
                ExecServerError::HttpRequest(_),
            )) => true,
            StreamableHttpError::Client(StreamableHttpClientAdapterError::HttpRequest(
                ExecServerError::Server { code, message, .. },
            )) => {
                *code == JSON_RPC_INTERNAL_ERROR_CODE && message.starts_with("http/request failed:")
            }
            StreamableHttpError::Client(StreamableHttpClientAdapterError::HttpRequest(
                ExecServerError::Protocol(message),
            )) => message.starts_with("http response stream `") && message.contains("` failed:"),
            StreamableHttpError::Client(
                StreamableHttpClientAdapterError::UnexpectedHttpStatus { status, .. },
            ) => is_retryable_http_status(*status),
            StreamableHttpError::AuthRequired(_)
            | StreamableHttpError::InsufficientScope(_)
            | StreamableHttpError::SessionExpired
            | StreamableHttpError::UnexpectedContentType(_)
            | StreamableHttpError::ServerDoesNotSupportSse
            | StreamableHttpError::Deserialize(_)
            | StreamableHttpError::Client(StreamableHttpClientAdapterError::SessionExpired404)
            | StreamableHttpError::Client(StreamableHttpClientAdapterError::Header(_))
            | StreamableHttpError::UnexpectedServerResponse(_) => false,
            _ => false,
        }
    }
}

// RMCP wraps transport failures rather than always exposing them through source().
// Keep retry advice attached to that error, across both handshake and read paths.
pub(super) fn retry_delay(error: &(dyn std::error::Error + 'static), fallback_ms: u64) -> Duration {
    fn advice(error: &(dyn std::error::Error + 'static)) -> Option<Duration> {
        if let Some(StreamableHttpClientAdapterError::UnexpectedHttpStatus {
            retry_after,
            retry_after_received_at,
            ..
        }) = error.downcast_ref::<StreamableHttpClientAdapterError>()
        {
            // Keep durations representable even when an absolute deadline would
            // overflow; sleep_with_retry_deadline handles that case safely.
            return retry_after
                .map(|delay| delay.saturating_sub(retry_after_received_at.elapsed()));
        }
        if let Some(StreamableHttpError::Client(error)) =
            error.downcast_ref::<StreamableHttpError<StreamableHttpClientAdapterError>>()
        {
            return advice(error);
        }
        if let Some(rmcp::service::ClientInitializeError::TransportError { error, .. }) =
            error.downcast_ref::<rmcp::service::ClientInitializeError>()
        {
            return advice(error.error.as_ref());
        }
        if let Some(rmcp::service::ServiceError::TransportSend(error)) =
            error.downcast_ref::<rmcp::service::ServiceError>()
        {
            return advice(error.error.as_ref());
        }
        error.source().and_then(advice)
    }
    Duration::from_millis(fallback_ms).max(advice(error).unwrap_or_default())
}

pub(super) async fn sleep_with_retry_deadline(delay: Duration, deadline: Option<Instant>) -> bool {
    let now = Instant::now();
    let wake_at = now.checked_add(delay);
    if let Some(deadline) = deadline {
        if now >= deadline {
            return false;
        }
        if let Some(wake_at) = wake_at.filter(|&wake_at| wake_at < deadline) {
            time::sleep_until(wake_at).await;
            return Instant::now() < deadline;
        }
        time::sleep_until(deadline).await;
        false
    } else {
        match wake_at {
            Some(wake_at) => time::sleep_until(wake_at).await,
            None => std::future::pending::<()>().await,
        }
        true
    }
}

#[derive(Debug, thiserror::Error)]
#[error("handshaking with MCP server failed: {source}")]
pub(super) struct HandshakeError {
    #[source]
    pub(super) source: rmcp::service::ClientInitializeError,
}

