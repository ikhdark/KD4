use std::sync::Arc;

use rmcp::ClientHandler;
use rmcp::RoleClient;
use rmcp::model::CancelledNotificationParam;
use rmcp::model::ClientInfo;
use rmcp::model::CreateElicitationRequestParams;
use rmcp::model::CreateElicitationResult;
use rmcp::model::LoggingLevel;
use rmcp::model::LoggingMessageNotificationParam;
use rmcp::model::ProgressNotificationParam;
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
use crate::rmcp_client::SendToolListChanged;

#[derive(Clone)]
pub(crate) struct LoggingClientHandler {
    client_info: ClientInfo,
    send_elicitation: Arc<SendElicitation>,
    send_progress: Arc<SendProgress>,
    send_tool_list_changed: Arc<SendToolListChanged>,
}

impl LoggingClientHandler {
    pub(crate) fn new(
        client_info: ClientInfo,
        send_elicitation: SendElicitation,
        send_progress: SendProgress,
        send_tool_list_changed: SendToolListChanged,
    ) -> Self {
        Self {
            client_info,
            send_elicitation: Arc::new(send_elicitation),
            send_progress: Arc::new(send_progress),
            send_tool_list_changed: Arc::new(send_tool_list_changed),
        }
    }

    async fn handle_progress_notification(&self, params: ProgressNotificationParam) {
        info!(
            "MCP server progress notification (token: {:?}, progress: {}, total: {:?}, message: {:?})",
            params.progress_token, params.progress, params.total, params.message
        );
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
        request: CreateElicitationRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<CreateElicitationResult, rmcp::ErrorData> {
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
            "MCP server cancelled request (request_id: {}, reason: {:?})",
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
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        info!("MCP server resource list changed");
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.handle_tool_list_changed_notification().await;
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        info!("MCP server prompt list changed");
    }

    fn get_info(&self) -> ClientInfo {
        self.client_info.clone()
    }

    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let LoggingMessageNotificationParam {
            level,
            logger,
            data,
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

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::NumberOrString;
    use rmcp::model::ProgressToken;
    use std::sync::Mutex;

    #[tokio::test]
    async fn progress_notifications_are_forwarded_to_runtime_callback() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_by_callback = Arc::clone(&seen);
        let handler = LoggingClientHandler::new(
            ClientInfo::default(),
            Box::new(|_, _| Box::pin(async move { unreachable!() })),
            Box::new(move |params| {
                let seen = Arc::clone(&seen_by_callback);
                Box::pin(async move {
                    seen.lock().expect("progress callback lock").push(params);
                })
            }),
            Box::new(|| Box::pin(async {})),
        );
        let params = ProgressNotificationParam {
            progress_token: ProgressToken(NumberOrString::String("item-1".into())),
            progress: 3.0,
            total: Some(10.0),
            message: Some("working".to_string()),
        };

        handler.handle_progress_notification(params.clone()).await;

        assert_eq!(
            seen.lock().expect("progress callback lock").as_slice(),
            &[params]
        );
    }

    #[tokio::test]
    async fn tool_list_changed_notifications_are_forwarded_to_runtime_callback() {
        let notification_count = Arc::new(Mutex::new(0));
        let notification_count_by_callback = Arc::clone(&notification_count);
        let handler = LoggingClientHandler::new(
            ClientInfo::default(),
            Box::new(|_, _| Box::pin(async move { unreachable!() })),
            Box::new(|_| Box::pin(async {})),
            Box::new(move || {
                let notification_count = Arc::clone(&notification_count_by_callback);
                Box::pin(async move {
                    *notification_count.lock().expect("tool-list callback lock") += 1;
                })
            }),
        );

        handler.handle_tool_list_changed_notification().await;

        assert_eq!(
            *notification_count.lock().expect("tool-list callback lock"),
            1
        );
    }
}
