use std::sync::Arc;
use std::sync::Weak;

use axum::http::HeaderValue;
use codex_app_server_protocol::AttestationGenerateParams;
use codex_app_server_protocol::AttestationGenerateResponse;
use codex_app_server_protocol::ServerRequestPayload;
use codex_core::AttestationContext;
use codex_core::AttestationProvider;
use codex_core::GenerateAttestationFuture;
use serde::Serialize;
use tokio::time::Duration;
use tokio::time::timeout;
use tracing::warn;

use crate::outgoing_message::OutgoingMessageSender;
use crate::thread_state::ThreadStateManager;

const ATTESTATION_GENERATE_TIMEOUT: Duration = Duration::from_millis(100);

fn attestation_timeout_milliseconds(timeout_duration: Duration) -> u128 {
    timeout_duration.as_millis()
}

pub(crate) fn app_server_attestation_provider(
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
) -> Arc<dyn AttestationProvider> {
    Arc::new(AppServerAttestationProvider {
        outgoing: Arc::downgrade(&outgoing),
        thread_state_manager,
    })
}

struct AppServerAttestationProvider {
    outgoing: Weak<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
}

impl std::fmt::Debug for AppServerAttestationProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppServerAttestationProvider")
            .finish()
    }
}

impl AttestationProvider for AppServerAttestationProvider {
    fn supports_startup_requests(&self) -> bool {
        false
    }

    fn header_for_request(&self, context: AttestationContext) -> GenerateAttestationFuture<'_> {
        let Some(outgoing) = self.outgoing.upgrade() else {
            return Box::pin(async { None });
        };
        let thread_state_manager = self.thread_state_manager.clone();
        Box::pin(async move {
            request_attestation_header_value_with_timeout(
                outgoing,
                thread_state_manager,
                context.thread_id,
                ATTESTATION_GENERATE_TIMEOUT,
            )
            .await
            .and_then(|value| HeaderValue::from_bytes(value.as_bytes()).ok())
        })
    }
}

async fn request_attestation_header_value_with_timeout(
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
    thread_id: codex_protocol::ThreadId,
    timeout_duration: Duration,
) -> Option<String> {
    let connection_ids = thread_state_manager
        .attestation_capable_connections_for_thread(thread_id)
        .await;
    if connection_ids.is_empty() {
        return None;
    }
    let (request_id, rx) = outgoing
        .send_request_to_connections(
            Some(connection_ids.as_slice()),
            ServerRequestPayload::AttestationGenerate(AttestationGenerateParams {}),
            /*thread_id*/ None,
        )
        .await;

    let result = match timeout(timeout_duration, rx).await {
        Ok(Ok(Ok(result))) => result,
        Ok(Ok(Err(err))) => {
            warn!(
                code = err.code,
                message = %err.message,
                "attestation generation request failed"
            );
            return app_server_attestation_header_value(
                AppServerAttestationStatus::RequestFailed,
                /*token*/ None,
            );
        }
        Ok(Err(err)) => {
            warn!("attestation generation request canceled: {err}");
            return app_server_attestation_header_value(
                AppServerAttestationStatus::RequestCanceled,
                /*token*/ None,
            );
        }
        Err(_) => {
            let _canceled = outgoing.cancel_request(&request_id).await;
            warn!(
                timeout_milliseconds = attestation_timeout_milliseconds(timeout_duration),
                "attestation generation request timed out"
            );
            return app_server_attestation_header_value(
                AppServerAttestationStatus::Timeout,
                /*token*/ None,
            );
        }
    };

    match serde_json::from_value::<AttestationGenerateResponse>(result) {
        Ok(response) => app_server_attestation_header_value(
            AppServerAttestationStatus::Ok,
            Some(&response.token),
        ),
        Err(err) => {
            warn!("failed to deserialize attestation generation response: {err}");
            app_server_attestation_header_value(
                AppServerAttestationStatus::MalformedResponse,
                /*token*/ None,
            )
        }
    }
}

#[derive(Clone, Copy)]
enum AppServerAttestationStatus {
    Ok,
    Timeout,
    RequestFailed,
    RequestCanceled,
    MalformedResponse,
}

impl AppServerAttestationStatus {
    const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Timeout => 1,
            Self::RequestFailed => 2,
            Self::RequestCanceled => 3,
            Self::MalformedResponse => 4,
        }
    }
}

#[derive(Serialize)]
struct AppServerAttestationEnvelope<'a> {
    v: u8,
    s: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    t: Option<&'a str>,
}

fn app_server_attestation_header_value(
    status: AppServerAttestationStatus,
    token: Option<&str>,
) -> Option<String> {
    serde_json::to_string(&AppServerAttestationEnvelope {
        v: 1,
        s: status.code(),
        t: token,
    })
    .map_err(|err| warn!("failed to serialize app-server attestation envelope: {err}"))
    .ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::ATTESTATION_GENERATE_TIMEOUT;
    use super::AppServerAttestationStatus;
    use super::app_server_attestation_header_value;
    use super::attestation_timeout_milliseconds;
    use super::request_attestation_header_value_with_timeout;
    use crate::outgoing_message::ConnectionId;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use crate::outgoing_message::OutgoingMessageSender;
    use crate::thread_state::ConnectionCapabilities;
    use crate::thread_state::ThreadStateManager;
    use codex_app_server_protocol::ServerRequest;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio::time::Duration;
    use tokio::time::timeout;

    #[test]
    fn app_server_attestation_header_value_wraps_opaque_client_payloads() {
        assert_eq!(
            app_server_attestation_header_value(
                AppServerAttestationStatus::Ok,
                Some("v1.opaque-client-payload"),
            ),
            Some(r#"{"v":1,"s":0,"t":"v1.opaque-client-payload"}"#.to_string())
        );
    }

    #[test]
    fn app_server_attestation_header_value_reports_app_server_failures() {
        assert_eq!(
            app_server_attestation_header_value(
                AppServerAttestationStatus::Timeout,
                /*token*/ None,
            ),
            Some(r#"{"v":1,"s":1}"#.to_string())
        );
        assert_eq!(
            app_server_attestation_header_value(
                AppServerAttestationStatus::RequestFailed,
                /*token*/ None,
            ),
            Some(r#"{"v":1,"s":2}"#.to_string())
        );
        assert_eq!(
            app_server_attestation_header_value(
                AppServerAttestationStatus::RequestCanceled,
                /*token*/ None,
            ),
            Some(r#"{"v":1,"s":3}"#.to_string())
        );
        assert_eq!(
            app_server_attestation_header_value(
                AppServerAttestationStatus::MalformedResponse,
                /*token*/ None
            ),
            Some(r#"{"v":1,"s":4}"#.to_string())
        );
    }

    #[test]
    fn attestation_timeout_diagnostic_preserves_subsecond_deadline() {
        assert_eq!(
            attestation_timeout_milliseconds(ATTESTATION_GENERATE_TIMEOUT),
            100
        );
    }

    #[tokio::test]
    async fn attestation_uses_healthy_alternate_connection() {
        let (tx, mut rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_state_manager = ThreadStateManager::new();
        let thread_id = ThreadId::new();
        let stale_connection = ConnectionId(1);
        let healthy_connection = ConnectionId(2);

        for connection_id in [stale_connection, healthy_connection] {
            outgoing
                .connection_opened(connection_id, Arc::new(AtomicBool::new(true)))
                .await;
            thread_state_manager
                .connection_initialized(
                    connection_id,
                    ConnectionCapabilities {
                        request_attestation: true,
                        ..Default::default()
                    },
                )
                .await;
            assert!(
                thread_state_manager
                    .try_add_connection_to_thread(thread_id, connection_id)
                    .await
            );
        }

        let request_task = tokio::spawn(request_attestation_header_value_with_timeout(
            Arc::clone(&outgoing),
            thread_state_manager,
            thread_id,
            Duration::from_secs(1),
        ));

        let first = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("first attestation request should arrive")
            .expect("outgoing channel should stay open");
        let second = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("alternate attestation request should arrive")
            .expect("outgoing channel should stay open");
        let mut deliveries = [first, second]
            .into_iter()
            .map(|envelope| match envelope {
                OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::Request(request),
                    ..
                } => (connection_id, request),
                _ => panic!("attestation should use targeted request delivery"),
            })
            .collect::<Vec<_>>();
        deliveries.sort_by_key(|(connection_id, _)| connection_id.0);
        assert_eq!(deliveries[0].0, stale_connection);
        assert_eq!(deliveries[1].0, healthy_connection);
        let healthy_request_id = match &deliveries[1].1 {
            ServerRequest::AttestationGenerate { request_id, .. } => request_id.clone(),
            request => panic!("expected attestation request, got {request:?}"),
        };

        outgoing
            .notify_client_response(
                healthy_connection,
                healthy_request_id,
                json!({"token": "healthy-token"}),
            )
            .await;

        assert_eq!(
            timeout(Duration::from_secs(1), request_task)
                .await
                .expect("healthy alternate should resolve attestation promptly")
                .expect("attestation task should not panic"),
            Some(r#"{"v":1,"s":0,"t":"healthy-token"}"#.to_string())
        );
    }
}
