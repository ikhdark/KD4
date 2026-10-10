use std::sync::Arc;

use rmcp::ClientHandler;
use rmcp::RoleClient;
use rmcp::model::CancelledNotificationParam;
use rmcp::model::ClientInfo;
use rmcp::model::ConstString;
use rmcp::model::CreateElicitationRequestParams;
use rmcp::model::CreateElicitationResult;
use rmcp::model::CustomNotification;
use rmcp::model::LoggingLevel;
use rmcp::model::LoggingMessageNotificationParam;
use rmcp::model::ProgressNotificationMethod;
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
    use crate::elicitation_client_service::ElicitationClientService;
    use crate::rmcp_client::ElicitationPauseRegistry;
    use rmcp::handler::server::ServerHandler;
    use rmcp::model::NumberOrString;
    use rmcp::model::ProgressToken;
    use rmcp::service::RoleServer;
    use rmcp::service::RunningService;
    use rmcp::service::serve_client;
    use rmcp::service::serve_server;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// A server with no behaviour of its own; the tests drive its peer handle.
    struct SilentServer;

    impl ServerHandler for SilentServer {}

    /// Connects the client service the runtime serves to an in-memory server, so a server
    /// notification has to cross the transport and the `ClientHandler` dispatch to be seen.
    async fn connect(
        send_progress: SendProgress,
        send_tool_list_changed: SendToolListChanged,
    ) -> (
        RunningService<RoleServer, SilentServer>,
        RunningService<RoleClient, ElicitationClientService>,
    ) {
        let client_service = ElicitationClientService::new(
            ClientInfo::default(),
            Box::new(|_, _| Box::pin(async move { unreachable!() })),
            send_progress,
            send_tool_list_changed,
            ElicitationPauseRegistry::default(),
        );
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (server, client) = tokio::join!(
            serve_server(SilentServer, server_io),
            serve_client(client_service, client_io)
        );
        (
            server.expect("in-memory server should initialize"),
            client.expect("in-memory client should initialize"),
        )
    }

    #[tokio::test]
    async fn progress_notifications_are_forwarded_to_runtime_callback() {
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<ProgressNotificationParam>();
        let (server, client) = connect(
            Box::new(move |params| {
                let progress_tx = progress_tx.clone();
                Box::pin(async move {
                    progress_tx
                        .send(params)
                        .expect("progress receiver should stay open");
                })
            }),
            Box::new(|| Box::pin(async {})),
        )
        .await;
        let params = ProgressNotificationParam {
            progress_token: ProgressToken(NumberOrString::String("item-1".into())),
            progress: 3.0,
            total: Some(10.0),
            message: Some("working".to_string()),
        };

        server
            .notify_progress(params.clone())
            .await
            .expect("server should send the progress notification");

        let forwarded = tokio::time::timeout(Duration::from_secs(10), progress_rx.recv())
            .await
            .expect("progress notification should reach the runtime callback")
            .expect("progress callback should stay registered");
        assert_eq!(forwarded, params);

        client.cancel().await.expect("client should shut down");
        server.cancel().await.expect("server should shut down");
        assert!(
            progress_rx.try_recv().is_err(),
            "one notification is forwarded exactly once"
        );
    }

    #[tokio::test]
    async fn tool_list_changed_notifications_are_forwarded_to_runtime_callback() {
        let (tool_list_tx, mut tool_list_rx) = mpsc::unbounded_channel::<()>();
        let (server, client) = connect(
            Box::new(|_| Box::pin(async {})),
            Box::new(move || {
                let tool_list_tx = tool_list_tx.clone();
                Box::pin(async move {
                    tool_list_tx
                        .send(())
                        .expect("tool-list receiver should stay open");
                })
            }),
        )
        .await;

        server
            .notify_tool_list_changed()
            .await
            .expect("server should send the tool-list notification");

        tokio::time::timeout(Duration::from_secs(10), tool_list_rx.recv())
            .await
            .expect("tool-list notification should reach the runtime callback")
            .expect("tool-list callback should stay registered");

        client.cancel().await.expect("client should shut down");
        server.cancel().await.expect("server should shut down");
        assert!(
            tool_list_rx.try_recv().is_err(),
            "one notification is forwarded exactly once"
        );
    }
}
