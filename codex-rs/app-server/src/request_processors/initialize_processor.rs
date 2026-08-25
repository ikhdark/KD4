use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use axum::http::HeaderValue;
use codex_analytics::AppServerRpcTransport;
use codex_app_server_protocol::ServerLocalWatermark;
use codex_app_server_protocol::WindowsWorldWritableWarningNotification;
use codex_login::default_client::CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR;
use codex_login::default_client::SetOriginatorError;
use codex_login::default_client::USER_AGENT_SUFFIX;
use codex_login::default_client::get_codex_user_agent;
use codex_login::default_client::set_default_client_residency_requirement;
use codex_login::default_client::set_default_originator;

use super::*;
use crate::message_processor::ConnectionSessionState;
use crate::message_processor::InitializedConnectionSessionState;

const NON_ORIGINATING_CLIENT_NAMES: &[&str] = &["codex-backend"];
const LOCAL_WATERMARK_VERSION: &str = "kd4";
const LOCAL_WATERMARK_LABEL: &str = "Codex KD4";

#[derive(Clone)]
pub(crate) struct InitializeRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    analytics_events_client: AnalyticsEventsClient,
    config: Arc<Config>,
    config_warnings: Arc<Vec<ConfigWarningNotification>>,
    rpc_transport: AppServerRpcTransport,
    experimental_api_mode: Arc<Mutex<Option<bool>>>,
}

impl InitializeRequestProcessor {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        analytics_events_client: AnalyticsEventsClient,
        config: Arc<Config>,
        config_warnings: Vec<ConfigWarningNotification>,
        rpc_transport: AppServerRpcTransport,
    ) -> Self {
        Self {
            outgoing,
            analytics_events_client,
            config,
            config_warnings: Arc::new(config_warnings),
            rpc_transport,
            experimental_api_mode: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) async fn initialize(
        &self,
        connection_id: ConnectionId,
        request_id: RequestId,
        params: InitializeParams,
        session: &ConnectionSessionState,
        // `Some(...)` means the caller wants initialize to immediately mark the
        // connection outbound-ready. Websocket JSON-RPC calls pass `None` so
        // lib.rs can deliver connection-scoped initialize notifications first.
        outbound_initialized: Option<&AtomicBool>,
    ) -> Result<bool, JSONRPCErrorError> {
        let connection_request_id = ConnectionRequestId {
            connection_id,
            request_id,
        };
        if session.initialized() {
            return Err(invalid_request("Already initialized"));
        }

        let analytics_initialize_params = params.clone();
        let capabilities = params.capabilities.unwrap_or_default();
        let experimental_api_enabled = capabilities.experimental_api;
        let request_attestation = capabilities.request_attestation;
        let supports_openai_form_elicitation = capabilities.mcp_server_openai_form_elicitation;
        let opt_out_notification_methods = capabilities
            .opt_out_notification_methods
            .unwrap_or_default();
        let ClientInfo {
            name,
            title: _title,
            version,
        } = params.client_info;
        // Validate before committing; set_default_originator validates while
        // mutating process-global metadata.
        if HeaderValue::from_str(&name).is_err() {
            return Err(invalid_request(format!(
                "Invalid clientInfo.name: '{name}'. Must be a valid HTTP header value."
            )));
        }
        let originator = name.clone();
        let user_agent_suffix = format!("{name}; {version}");
        let mutates_global_identity = !NON_ORIGINATING_CLIENT_NAMES.contains(&name.as_str());
        let codex_home = self.config.codex_home.clone();
        {
            let mut experimental_api_mode = self
                .experimental_api_mode
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(selected) = *experimental_api_mode
                && selected != experimental_api_enabled
            {
                return Err(invalid_request(format!(
                    "experimental_api must match the first initialized connection (expected {selected})"
                )));
            }
            if session
                .initialize(InitializedConnectionSessionState {
                    experimental_api_enabled,
                    opted_out_notification_methods: opt_out_notification_methods
                        .into_iter()
                        .collect(),
                    app_server_client_name: name.clone(),
                    client_version: version,
                    request_attestation,
                    supports_openai_form_elicitation,
                })
                .is_err()
            {
                return Err(invalid_request("Already initialized"));
            }
            experimental_api_mode.get_or_insert(experimental_api_enabled);
        }

        if mutates_global_identity {
            // Only real client initialization may mutate process-global client metadata.
            let originator_overridden =
                std::env::var_os(CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR).is_some();
            match set_default_originator(originator.clone()) {
                Ok(()) => {
                    // Keep the first originating client's User-Agent suffix paired with its
                    // originator. Environment-owned originators must not inherit a client suffix.
                    if !originator_overridden && let Ok(mut suffix) = USER_AGENT_SUFFIX.lock() {
                        *suffix = Some(user_agent_suffix);
                    }
                }
                Err(SetOriginatorError::InvalidHeaderValue) => {
                    tracing::warn!(
                        client_info_name = %name,
                        "validated clientInfo.name was rejected while setting originator"
                    );
                }
                Err(SetOriginatorError::AlreadyInitialized) => {
                    // The first originating client owns both process-global identity
                    // components. Later clients must not replace only the suffix and
                    // create a mixed originator/User-Agent identity.
                }
            }
        }
        self.analytics_events_client.track_initialize(
            connection_id.0,
            analytics_initialize_params,
            originator,
            self.rpc_transport,
        );
        set_default_client_residency_requirement(self.config.enforce_residency.value());
        let user_agent = get_codex_user_agent();
        let mut enabled_features: Vec<_> = self
            .config
            .features
            .enabled_features()
            .into_iter()
            .map(|feature| feature.key().to_string())
            .collect();
        enabled_features.sort();
        let response = InitializeResponse {
            user_agent,
            codex_home,
            platform_family: std::env::consts::FAMILY.to_string(),
            platform_os: std::env::consts::OS.to_string(),
            build_info: Some(crate::build_info::server_build_info(
                codex_utils_build_info::BuildInfo::current(),
            )),
            runtime_info: Some(crate::runtime_provenance::current()),
            server_capabilities: Some(codex_app_server_protocol::ServerCapabilities {
                enabled_features,
            }),
            local_watermark: Some(ServerLocalWatermark {
                version: LOCAL_WATERMARK_VERSION.to_string(),
                label: LOCAL_WATERMARK_LABEL.to_string(),
                detail: "Local Codex KD4 runtime".to_string(),
            }),
        };

        self.outgoing
            .send_response(connection_request_id, response)
            .await;

        if let Some(outbound_initialized) = outbound_initialized {
            outbound_initialized.store(true, Ordering::Release);
            return Ok(true);
        }

        Ok(false)
    }

    pub(crate) async fn send_initialize_notifications_to_connection(
        &self,
        connection_id: ConnectionId,
    ) {
        for notification in self.config_warnings.iter().cloned() {
            self.outgoing
                .send_server_notification_to_connections(
                    &[connection_id],
                    ServerNotification::ConfigWarning(notification),
                )
                .await;
        }
        self.spawn_windows_world_writable_warning(vec![connection_id]);
    }

    pub(crate) async fn send_initialize_notifications(&self) {
        for notification in self.config_warnings.iter().cloned() {
            self.outgoing
                .send_server_notification(ServerNotification::ConfigWarning(notification))
                .await;
        }
        self.spawn_windows_world_writable_warning(Vec::new());
    }

    fn spawn_windows_world_writable_warning(&self, connection_ids: Vec<ConnectionId>) {
        let permission_profile = self.config.permissions.effective_permission_profile();
        let should_scan =
            codex_core::windows_sandbox::windows_sandbox_level_from_config(&self.config)
                != codex_protocol::config_types::WindowsSandboxLevel::Disabled
                && permission_profile.file_system_sandbox_policy().kind
                    == codex_protocol::permissions::FileSystemSandboxKind::Restricted
                && !self
                    .config
                    .notices
                    .hide_world_writable_warning
                    .unwrap_or(false);
        if !should_scan {
            return;
        }

        let codex_home = self.config.codex_home.clone();
        let cwd = self.config.cwd.clone();
        let outgoing = Arc::clone(&self.outgoing);
        tokio::spawn(async move {
            let details = match tokio::task::spawn_blocking(move || {
                codex_windows_sandbox::world_writable_warning_details(codex_home, cwd)
            })
            .await
            {
                Ok(details) => details,
                Err(err) => {
                    tracing::warn!("world-writable scan task failed: {err}");
                    Some((Vec::new(), 0, true))
                }
            };
            if let Some(details) = details {
                send_windows_world_writable_warning_details(&outgoing, &connection_ids, details)
                    .await;
            }
        });
    }

    pub(crate) fn track_initialized_request(
        &self,
        connection_id: ConnectionId,
        request_id: RequestId,
        request: &ClientRequest,
    ) {
        self.analytics_events_client
            .track_request(connection_id.0, request_id, request);
    }

    pub(crate) fn track_initialized_request_error(
        &self,
        connection_id: ConnectionId,
        request_id: RequestId,
    ) {
        self.analytics_events_client.track_error_response(
            connection_id.0,
            request_id,
            /*error_type*/ None,
        );
    }
}

async fn send_windows_world_writable_warning_details(
    outgoing: &OutgoingMessageSender,
    connection_ids: &[ConnectionId],
    (sample_paths, extra_count, failed_scan): (Vec<String>, usize, bool),
) {
    tracing::warn!(
        ?sample_paths,
        extra_count,
        failed_scan,
        "world-writable warning"
    );
    outgoing
        .send_server_notification_to_connections(
            connection_ids,
            ServerNotification::WindowsWorldWritableWarning(
                WindowsWorldWritableWarningNotification {
                    sample_paths,
                    extra_count,
                    failed_scan,
                },
            ),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::send_windows_world_writable_warning_details;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use crate::outgoing_message::OutgoingMessageSender;
    use codex_app_server_protocol::ServerNotification;
    use codex_app_server_transport::ConnectionId;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn world_writable_warning_is_sent_to_initializing_connection() {
        let (tx, mut rx) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let connection_id = ConnectionId(7);

        send_windows_world_writable_warning_details(
            &outgoing,
            &[connection_id],
            (vec!["C:\\shared".to_string()], 2, false),
        )
        .await;

        let OutgoingEnvelope::ToConnection {
            connection_id: actual_connection_id,
            message:
                OutgoingMessage::AppServerNotification(ServerNotification::WindowsWorldWritableWarning(
                    notification,
                )),
            ..
        } = rx.recv().await.expect("warning notification")
        else {
            panic!("expected a connection-scoped world-writable warning");
        };
        assert_eq!(actual_connection_id, connection_id);
        assert_eq!(notification.sample_paths, vec!["C:\\shared"]);
        assert_eq!(notification.extra_count, 2);
        assert!(!notification.failed_scan);
    }
}
