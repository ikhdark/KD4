use std::sync::Arc;
use std::sync::RwLock;

use codex_app_server_protocol::ChatgptAuthTokensRefreshParams;
use codex_app_server_protocol::ChatgptAuthTokensRefreshReason;
use codex_app_server_protocol::ChatgptAuthTokensRefreshResponse;
use codex_app_server_protocol::ServerRequestPayload;
use codex_login::CodexAuth;
use codex_login::ExternalAuthFuture;
use codex_login::auth::ExternalAuth;
use codex_login::auth::ExternalAuthRefreshContext;
use codex_login::auth::ExternalAuthRefreshReason;
use tokio::time::Duration;
use tokio::time::timeout;

use crate::outgoing_message::OutgoingMessageSender;

const EXTERNAL_AUTH_REFRESH_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct ExternalAuthBridge {
    outgoing: Arc<OutgoingMessageSender>,
    auth: RwLock<CodexAuth>,
}

impl ExternalAuthBridge {
    pub(crate) fn new(outgoing: Arc<OutgoingMessageSender>, auth: CodexAuth) -> Self {
        Self {
            outgoing,
            auth: RwLock::new(auth),
        }
    }

    async fn refresh(&self, context: ExternalAuthRefreshContext) -> std::io::Result<CodexAuth> {
        let reason = match context.reason {
            ExternalAuthRefreshReason::Unauthorized => ChatgptAuthTokensRefreshReason::Unauthorized,
        };
        let params = ChatgptAuthTokensRefreshParams {
            reason,
            previous_account_id: context.previous_account_id,
        };

        let request = self.outgoing.send_request_to_connections_and_wait(
            None,
            ServerRequestPayload::ChatgptAuthTokensRefresh(params),
            None,
        );
        let result = match timeout(EXTERNAL_AUTH_REFRESH_TIMEOUT, request).await {
            Ok(result) => {
                let result = result.map_err(|err| {
                    std::io::Error::other(format!("auth refresh request canceled: {err}"))
                })?;
                result.map_err(|err| {
                    std::io::Error::other(format!(
                        "auth refresh request failed: code={} message={}",
                        err.code, err.message
                    ))
                })?
            }
            Err(_) => {
                return Err(std::io::Error::other(format!(
                    "auth refresh request timed out after {}s",
                    EXTERNAL_AUTH_REFRESH_TIMEOUT.as_secs()
                )));
            }
        };

        let response: ChatgptAuthTokensRefreshResponse =
            serde_json::from_value(result).map_err(std::io::Error::other)?;
        let auth = CodexAuth::from_external_chatgpt_tokens(
            response.access_token.as_str(),
            response.chatgpt_account_id.as_str(),
            response.chatgpt_plan_type.as_deref(),
        )?;
        *self
            .auth
            .write()
            .map_err(|_| std::io::Error::other("external auth lock is poisoned"))? = auth.clone();
        Ok(auth)
    }
}

impl ExternalAuth for ExternalAuthBridge {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async {
            self.auth
                .read()
                .map(|auth| auth.clone())
                .map_err(|_| std::io::Error::other("external auth lock is poisoned"))
        })
    }

    fn refresh(&self, context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(ExternalAuthBridge::refresh(self, context))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outgoing_message::ConnectionId;
    use codex_app_server_protocol::ServerNotification;
    use codex_app_server_protocol::ThreadClosedNotification;
    use std::sync::atomic::AtomicBool;

    #[tokio::test(start_paused = true)]
    async fn auth_deadline_covers_delivery_and_reply_and_cleans_callbacks() {
        for saturated in [true, false] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            let outgoing = Arc::new(OutgoingMessageSender::new(
                tx,
                codex_analytics::AnalyticsEventsClient::disabled(),
            ));
            outgoing
                .connection_opened(ConnectionId(1), Arc::new(AtomicBool::new(true)))
                .await;
            if saturated {
                assert!(
                    outgoing.try_send_server_notification(ServerNotification::ThreadClosed(
                        ThreadClosedNotification {
                            thread_id: "blocker".to_string()
                        }
                    ))
                );
            }
            let bridge =
                ExternalAuthBridge::new(outgoing.clone(), CodexAuth::from_api_key("original"));
            let mut refresh = Box::pin(bridge.refresh(ExternalAuthRefreshContext {
                reason: ExternalAuthRefreshReason::Unauthorized,
                previous_account_id: None,
            }));
            assert!(futures::poll!(refresh.as_mut()).is_pending());
            assert_eq!(outgoing.pending_callback_count().await, 1);
            let delivery_delay = if saturated {
                let delay = Duration::from_secs(4);
                tokio::time::advance(delay).await;
                rx.recv().await.expect("blocker");
                assert!(futures::poll!(refresh.as_mut()).is_pending());
                delay
            } else {
                Duration::ZERO
            };
            tokio::time::advance(EXTERNAL_AUTH_REFRESH_TIMEOUT - delivery_delay).await;
            assert_eq!(
                refresh.await.expect_err("refresh deadline").to_string(),
                "auth refresh request timed out after 10s"
            );
            assert_eq!(outgoing.pending_callback_count().await, 0);
            rx.recv().await.expect("delivered request");
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn dropping_auth_refresh_cleans_a_delivered_callback() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        outgoing
            .connection_opened(ConnectionId(1), Arc::new(AtomicBool::new(true)))
            .await;
        let bridge = ExternalAuthBridge::new(outgoing.clone(), CodexAuth::from_api_key("original"));
        let mut refresh = Box::pin(bridge.refresh(ExternalAuthRefreshContext {
            reason: ExternalAuthRefreshReason::Unauthorized,
            previous_account_id: None,
        }));
        assert!(futures::poll!(refresh.as_mut()).is_pending());
        assert!(rx.try_recv().is_ok());
        assert_eq!(outgoing.pending_callback_count().await, 1);
        drop(refresh);
        assert_eq!(outgoing.pending_callback_count().await, 0);
    }
}
