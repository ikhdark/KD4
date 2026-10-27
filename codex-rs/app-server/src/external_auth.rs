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
use crate::transport::ConnectionId;

const EXTERNAL_AUTH_REFRESH_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct ExternalAuthBridge {
    outgoing: Arc<OutgoingMessageSender>,
    owner: ConnectionId,
    auth: RwLock<CodexAuth>,
}

impl ExternalAuthBridge {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        owner: ConnectionId,
        auth: CodexAuth,
    ) -> Self {
        Self {
            outgoing,
            owner,
            auth: RwLock::new(auth),
        }
    }

    async fn refresh(&self, context: ExternalAuthRefreshContext) -> std::io::Result<CodexAuth> {
        let reason = match context.reason {
            ExternalAuthRefreshReason::Unauthorized => ChatgptAuthTokensRefreshReason::Unauthorized,
        };
        let params = ChatgptAuthTokensRefreshParams {
            reason,
            previous_account_id: context.previous_account_id.clone(),
        };

        // Supplying external credentials explicitly transfers refresh ownership to
        // that connection. Reconnecting clients must supply credentials again;
        // subscriptions or being the first responder never confer that authority.
        let owners = [self.owner];
        let request = self.outgoing.send_request_to_connections_and_wait(
            Some(&owners),
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
        if let Some(expected) = context.previous_account_id.as_deref()
            && auth.get_account_id().as_deref() != Some(expected)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "authentication account changed during external refresh; log in explicitly",
            ));
        }
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

