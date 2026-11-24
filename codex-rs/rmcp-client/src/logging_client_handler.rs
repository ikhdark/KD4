use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use codex_protocol::mcp::decode_tool_call_progress_token;

use rmcp::ClientHandler;
use rmcp::RoleClient;
use rmcp::model::CancelledNotificationParam;
use rmcp::model::ClientConfig;
use rmcp::model::ConstString;
use rmcp::model::CustomNotification;
use rmcp::model::ElicitRequestParams;
use rmcp::model::ElicitResult;
// MCP logging is deprecated by SEP-2577, but servers still send it.
#[allow(deprecated)]
use rmcp::model::LoggingLevel;
#[allow(deprecated)]
use rmcp::model::LoggingMessageNotificationParam;
use rmcp::model::ProgressNotificationMethod;
use rmcp::model::NumberOrString;
use rmcp::model::ProgressNotificationParam;
use rmcp::model::ProgressToken;
use rmcp::model::ResourceUpdatedNotificationParam;
use rmcp::service::NotificationContext;
use rmcp::service::RequestContext;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::rmcp_client::Elicitation;
use crate::rmcp_client::SendElicitation;
use crate::rmcp_client::SendProgress;
use crate::rmcp_client::SendResourceChanged;
use crate::rmcp_client::SendToolListChanged;

// One table per transport session: rmcp's numeric tokens restart on reconnect.
// The async gate covers only send-and-register, not tool execution or UI I/O.
#[derive(Default)]
pub(crate) struct ProgressRoutes {
    pub(crate) registration: tokio::sync::Mutex<()>,
    routes: Mutex<HashMap<ProgressToken, ProgressToken>>,
}

impl ProgressRoutes {
    pub(crate) fn register(
        self: &Arc<Self>,
        wire: ProgressToken,
        item: ProgressToken,
    ) -> ProgressRouteLease {
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(wire.clone(), item);
        ProgressRouteLease {
            routes: Arc::clone(self),
            wire,
        }
    }

    async fn resolve(&self, wire: &ProgressToken) -> Option<ProgressToken> {
        let _registration = self.registration.lock().await;
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(wire)
            .cloned()
    }
}

pub(crate) struct ProgressRouteLease {
    routes: Arc<ProgressRoutes>,
    wire: ProgressToken,
}

impl Drop for ProgressRouteLease {
    fn drop(&mut self) {
        self.routes
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.wire);
    }
}

#[derive(Clone)]
pub(crate) struct LoggingClientHandler {
    client_info: ClientConfig,
    send_elicitation: Arc<SendElicitation>,
    send_progress: Arc<SendProgress>,
    send_tool_list_changed: Arc<SendToolListChanged>,
    send_resource_changed: Arc<SendResourceChanged>,
    pub(crate) progress_routes: Arc<ProgressRoutes>,
}

impl LoggingClientHandler {
    pub(crate) fn new(
        client_info: ClientConfig,
        send_elicitation: SendElicitation,
        send_progress: SendProgress,
        send_tool_list_changed: SendToolListChanged,
        send_resource_changed: SendResourceChanged,
    ) -> Self {
        Self {
            client_info,
            send_elicitation: Arc::new(send_elicitation),
            send_progress: Arc::new(send_progress),
            send_tool_list_changed: Arc::new(send_tool_list_changed),
            send_resource_changed: Arc::new(send_resource_changed),
            progress_routes: Arc::default(),
        }
    }

    async fn handle_progress_notification(&self, mut params: ProgressNotificationParam) {
        info!(
            "MCP server progress notification (token: {:?}, progress: {}, total: {:?}, message: {:?})",
            params.progress_token, params.progress, params.total, params.message
        );
        if let Some(token) = self.progress_routes.resolve(&params.progress_token).await {
            params.progress_token = token;
        } else if let NumberOrString::String(token) = &params.progress_token.0
            && decode_tool_call_progress_token(token).is_some()
        {
            // Codex item identities are local routing data, not wire authority.
            return;
        }
        (self.send_progress)(params).await;
    }

    async fn handle_tool_list_changed_notification(&self) {
        info!("MCP server tool list changed");
        (self.send_tool_list_changed)().await;
    }
}

impl ClientHandler for LoggingClientHandler {
    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        (self.send_elicitation)(context.id, Elicitation::Mcp(request))
            .await
            .map(Into::into)
            .map_err(|err| rmcp::ErrorData::internal_error(err.to_string(), None))
    }

    async fn on_cancelled(
        &self,
        params: CancelledNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        info!(
            "MCP server cancelled request (request_id: {:?}, reason: {:?})",
            params.request_id, params.reason
        );
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.handle_progress_notification(params).await;
    }

    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        info!("MCP server resource updated (uri: {})", params.uri);
        (self.send_resource_changed)().await;
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        info!("MCP server resource list changed");
        (self.send_resource_changed)().await;
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.handle_tool_list_changed_notification().await;
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        info!("MCP server prompt list changed");
    }

    // serde_json's workspace-enabled arbitrary_precision feature keeps rmcp's
    // untagged notification enum from decoding the float fields of a progress
    // notification, so one arrives here instead of at `on_progress`.
    async fn on_custom_notification(
        &self,
        notification: CustomNotification,
        _context: NotificationContext<RoleClient>,
    ) {
        if notification.method != ProgressNotificationMethod::VALUE {
            return;
        }
        match notification.params_as::<ProgressNotificationParam>() {
            Ok(Some(params)) => self.handle_progress_notification(params).await,
            Ok(None) => {}
            Err(err) => warn!("ignoring malformed MCP progress notification: {err}"),
        }
    }

    fn get_info(&self) -> ClientConfig {
        self.client_info.clone()
    }

    #[allow(deprecated)]
    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let LoggingMessageNotificationParam {
            level,
            logger,
            data,
            ..
        } = params;
        let logger = logger.as_deref();
        match level {
            LoggingLevel::Emergency
            | LoggingLevel::Alert
            | LoggingLevel::Critical
            | LoggingLevel::Error => {
                error!(
                    "MCP server log message (level: {:?}, logger: {:?}, data: {})",
                    level, logger, data
                );
            }
            LoggingLevel::Warning => {
                warn!(
                    "MCP server log message (level: {:?}, logger: {:?}, data: {})",
                    level, logger, data
                );
            }
            LoggingLevel::Notice | LoggingLevel::Info => {
                info!(
                    "MCP server log message (level: {:?}, logger: {:?}, data: {})",
                    level, logger, data
                );
            }
            LoggingLevel::Debug => {
                debug!(
                    "MCP server log message (level: {:?}, logger: {:?}, data: {})",
                    level, logger, data
                );
            }
        }
    }
}

