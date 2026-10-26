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

    fn wait_until_ready(
        &self,
        context: AttestationContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(
            self.thread_state_manager
                .wait_for_thread_subscriber(context.thread_id),
        )
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
    let request = async {
        let connection_ids = thread_state_manager
            .attestation_capable_connections_for_thread(thread_id)
            .await;
        if connection_ids.is_empty() {
            return None;
        }
        Some(
            outgoing
                .send_request_to_connections_and_wait(
                    Some(connection_ids.as_slice()),
                    ServerRequestPayload::AttestationGenerate(AttestationGenerateParams {}),
                    /*thread_id*/ None,
                )
                .await,
        )
    };
    let result = match timeout(timeout_duration, request).await {
        Ok(None) => return None,
        Ok(Some(Ok(Ok(result)))) => result,
        Ok(Some(Ok(Err(err)))) => {
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
        Ok(Some(Err(err))) => {
            warn!("attestation generation request canceled: {err}");
            return app_server_attestation_header_value(
                AppServerAttestationStatus::RequestCanceled,
                /*token*/ None,
            );
        }
        Err(_) => {
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

