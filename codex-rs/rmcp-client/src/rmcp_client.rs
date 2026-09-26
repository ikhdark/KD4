use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use codex_api::SharedAuthProvider;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::McpServerEnvVar;
use codex_exec_server::HttpClient;
use futures::FutureExt;
use futures::future::BoxFuture;
use http::HeaderMap;
use http::header::AUTHORIZATION;
use oauth2::TokenResponse;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::CancelledNotification;
use rmcp::model::CancelledNotificationParam;
use rmcp::model::ClientNotification;
use rmcp::model::ClientRequest;
use rmcp::model::CreateElicitationRequestParams;
use rmcp::model::CreateElicitationResult;
use rmcp::model::CustomNotification;
use rmcp::model::CustomRequest;
use rmcp::model::ElicitationAction;
use rmcp::model::Extensions;
use rmcp::model::InitializeRequestParams;
use rmcp::model::InitializeResult;
use rmcp::model::ListResourceTemplatesRequest;
use rmcp::model::ListResourceTemplatesResult;
use rmcp::model::ListResourcesRequest;
use rmcp::model::ListResourcesResult;
use rmcp::model::ListToolsRequest;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ProgressNotificationParam;
use rmcp::model::ReadResourceRequest;
use rmcp::model::ReadResourceRequestParams;
use rmcp::model::ReadResourceResult;
use rmcp::model::RequestId;
use rmcp::model::RequestParamsMeta;
use rmcp::model::ServerResult;
use rmcp::model::Tool;
use rmcp::service::RoleClient;
use rmcp::service::RunningService;
use rmcp::service::{self};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::auth::AuthClient;
use rmcp::transport::auth::AuthError;
use rmcp::transport::auth::OAuthState;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_client::StreamableHttpError;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tokio::time;
use tokio::time::Instant;
use tracing::instrument;
use tracing::warn;

use crate::elicitation_client_service::ElicitationClientService;
use crate::http_client_adapter::StreamableHttpClientAdapter;
use crate::http_client_adapter::StreamableHttpClientAdapterError;
use crate::load_oauth_tokens;
use crate::oauth::OAuthPersistor;
use crate::oauth::StoredOAuthTokens;
use crate::oauth_http_client::OAuthHttpClientAdapter;
use crate::stdio_server_launcher::StdioServerCommand;
use crate::stdio_server_launcher::StdioServerLauncher;
use crate::stdio_server_launcher::StdioServerProcessHandle;
use crate::stdio_server_launcher::StdioServerTransport;
use crate::utils::build_default_headers;
use codex_config::types::OAuthCredentialsStoreMode;

#[path = "streamable_http_retry.rs"]
mod streamable_http_retry;

use self::streamable_http_retry::HandshakeError;
use self::streamable_http_retry::STREAMABLE_HTTP_RETRY_DELAYS_MS;

enum PendingTransport {
    Stdio {
        transport: StdioServerTransport,
    },
    StreamableHttp {
        transport: StreamableHttpClientTransport<StreamableHttpClientAdapter>,
    },
    StreamableHttpWithOAuth {
        transport: StreamableHttpClientTransport<AuthClient<StreamableHttpClientAdapter>>,
        oauth_persistor: OAuthPersistor,
    },
}

enum ClientState {
    Connecting {
        transport: Option<PendingTransport>,
    },
    Ready {
        service: Arc<RunningService<RoleClient, ElicitationClientService>>,
        oauth: Option<OAuthPersistor>,
    },
    Closed,
}

#[derive(Clone)]
enum TransportRecipe {
    Stdio {
        command: StdioServerCommand,
        launcher: Arc<dyn StdioServerLauncher>,
    },
    StreamableHttp {
        server_name: String,
        codex_home: PathBuf,
        url: String,
        bearer_token: Option<String>,
        http_headers: Option<HashMap<String, String>>,
        env_http_headers: Option<HashMap<String, String>>,
        store_mode: OAuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
        http_client: Arc<dyn HttpClient>,
        auth_provider: Option<SharedAuthProvider>,
    },
}

#[derive(Clone)]
struct InitializeContext {
    timeout: Option<Duration>,
    client_service: ElicitationClientService,
}

#[derive(Clone)]
pub(crate) struct ElicitationPauseState {
    timing: watch::Sender<ElicitationTiming>,
}

#[derive(Clone, Copy)]
struct ElicitationTiming {
    active_count: usize,
    active_elapsed: Duration,
    active_since: Instant,
}

impl ElicitationTiming {
    fn active_time(&self) -> Duration {
        self.active_elapsed
            + if self.active_count == 0 {
                self.active_since.elapsed()
            } else {
                Duration::ZERO
            }
    }
}

impl ElicitationPauseState {
    fn new() -> Self {
        let (timing, _rx) = watch::channel(ElicitationTiming {
            active_count: 0,
            active_elapsed: Duration::ZERO,
            active_since: Instant::now(),
        });
        Self { timing }
    }

    pub(crate) fn enter(&self) -> ElicitationPauseGuard {
        self.timing.send_modify(|timing| {
            if timing.active_count == 0 {
                timing.active_elapsed = timing.active_time();
            }
            timing.active_count += 1;
        });
        ElicitationPauseGuard {
            pause_state: self.clone(),
        }
    }

    fn subscribe(&self) -> watch::Receiver<ElicitationTiming> {
        self.timing.subscribe()
    }
}

pub(crate) struct ElicitationPauseGuard {
    pause_state: ElicitationPauseState,
}

impl Drop for ElicitationPauseGuard {
    fn drop(&mut self) {
        self.pause_state.timing.send_modify(|timing| {
            timing.active_count -= 1;
            if timing.active_count == 0 {
                timing.active_since = Instant::now();
            }
        });
    }
}

async fn active_time_timeout<T, Fut>(
    duration: Duration,
    mut pause_state: watch::Receiver<ElicitationTiming>,
    operation: Fut,
) -> std::result::Result<T, ()>
where
    Fut: Future<Output = T>,
{
    let started = pause_state.borrow_and_update().active_time();
    tokio::pin!(operation);
    loop {
        let (active_count, active_time) = {
            let timing = pause_state.borrow_and_update();
            (timing.active_count, timing.active_time())
        };
        let remaining = duration.saturating_sub(active_time.saturating_sub(started));
        if remaining.is_zero() {
            return Err(());
        }
        tokio::select! {
            result = &mut operation => return Ok(result),
            _ = time::sleep(remaining), if active_count == 0 => {},
            changed = pause_state.changed() => {
                if changed.is_err() {
                    let remaining = duration.saturating_sub(
                        pause_state.borrow().active_time().saturating_sub(started)
                    );
                    return time::timeout(remaining, operation).await.map_err(|_| ());
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum ClientOperationError {
    #[error(transparent)]
    Service(#[from] rmcp::service::ServiceError),
    #[error("timed out awaiting {label} after {duration:.0?}")]
    Timeout { label: String, duration: Duration },
}

#[derive(Debug, thiserror::Error)]
#[error("timed out handshaking with MCP server after {duration:?}")]
struct InitializeTimeoutError {
    duration: Duration,
}

fn initialize_timeout_error(duration: Duration) -> anyhow::Error {
    anyhow::Error::new(InitializeTimeoutError { duration })
}

pub fn is_timeout_error(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source
            .downcast_ref::<ClientOperationError>()
            .is_some_and(|error| matches!(error, ClientOperationError::Timeout { .. }))
            || source.downcast_ref::<InitializeTimeoutError>().is_some()
    })
}

type RunningClientService = RunningService<RoleClient, ElicitationClientService>;

#[derive(Clone)]
struct TrackedRequest {
    service: Arc<RunningClientService>,
    id: RequestId,
}

#[derive(Clone, Default)]
struct OperationRequestTracker {
    current: Arc<StdMutex<Option<TrackedRequest>>>,
}

impl OperationRequestTracker {
    fn register(&self, service: Arc<RunningClientService>, id: RequestId) {
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(TrackedRequest { service, id });
    }

    fn clear(&self, id: &RequestId) {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.as_ref().is_some_and(|request| &request.id == id) {
            *current = None;
        }
    }

    async fn cancel_current(&self, reason: &str) {
        let request = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(TrackedRequest { service, id }) = request else {
            return;
        };
        let notification = ClientNotification::CancelledNotification(CancelledNotification::new(
            CancelledNotificationParam {
                request_id: id,
                reason: Some(reason.to_string()),
            },
        ));
        if let Err(error) = service.send_notification(notification).await {
            warn!("failed to send MCP request cancellation: {error}");
        }
    }
}

struct OperationCancellationGuard {
    tracker: Option<OperationRequestTracker>,
    runtime: Option<tokio::runtime::Handle>,
}

impl OperationCancellationGuard {
    fn new(tracker: OperationRequestTracker) -> Self {
        Self {
            tracker: Some(tracker),
            runtime: tokio::runtime::Handle::try_current().ok(),
        }
    }

    fn disarm(&mut self) {
        self.tracker = None;
    }
}

impl Drop for OperationCancellationGuard {
    fn drop(&mut self) {
        let Some(tracker) = self.tracker.take() else {
            return;
        };
        let Some(runtime) = self
            .runtime
            .take()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        else {
            return;
        };
        runtime.spawn(async move {
            tracker.cancel_current("request cancelled").await;
        });
    }
}

async fn send_tracked_request(
    service: Arc<RunningClientService>,
    tracker: OperationRequestTracker,
    request: ClientRequest,
    options: rmcp::service::PeerRequestOptions,
) -> std::result::Result<ServerResult, rmcp::service::ServiceError> {
    let handle = service
        .peer()
        .send_request_with_option(request, options)
        .await?;
    let id = handle.id.clone();
    tracker.register(service, id.clone());
    let response = handle.await_response().await;
    tracker.clear(&id);
    response
}

#[derive(Debug, Clone, PartialEq)]
pub enum Elicitation {
    Mcp(CreateElicitationRequestParams),
    OpenAiForm {
        meta: Option<serde_json::Value>,
        message: String,
        requested_schema: serde_json::Value,
    },
}

impl Elicitation {
    pub fn meta(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        match self {
            Self::Mcp(request) => request.meta().map(|meta| &meta.0),
            Self::OpenAiForm { meta, .. } => meta.as_ref().and_then(serde_json::Value::as_object),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    pub content: Option<serde_json::Value>,
    #[serde(rename = "_meta")]
    pub meta: Option<serde_json::Value>,
}

impl From<CreateElicitationResult> for ElicitationResponse {
    fn from(value: CreateElicitationResult) -> Self {
        Self {
            action: value.action,
            content: value.content,
            meta: value.meta.map(|meta| serde_json::Value::Object(meta.0)),
        }
    }
}

impl From<ElicitationResponse> for CreateElicitationResult {
    /// Preserves object metadata supported by RMCP. Non-object metadata is omitted;
    /// the runtime's custom response serializer supports arbitrary JSON metadata.
    fn from(value: ElicitationResponse) -> Self {
        Self {
            action: value.action,
            content: value.content,
            meta: value.meta.and_then(|meta| match meta {
                serde_json::Value::Object(meta) => Some(rmcp::model::Meta(meta)),
                _ => None,
            }),
        }
    }
}

/// Interface for sending elicitation requests to the UI and awaiting a response.
pub type SendElicitation = Box<
    dyn Fn(RequestId, Elicitation) -> BoxFuture<'static, Result<ElicitationResponse>> + Send + Sync,
>;

/// Interface for forwarding MCP progress notifications to the runtime.
pub type SendProgress =
    Box<dyn Fn(ProgressNotificationParam) -> BoxFuture<'static, ()> + Send + Sync>;

/// Interface for forwarding `notifications/tools/list_changed` to the catalog owner.
pub type SendToolListChanged = Box<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

pub struct ToolWithConnectorId {
    pub tool: Tool,
    pub connector_id: Option<String>,
    pub connector_name: Option<String>,
    pub connector_description: Option<String>,
}

pub struct ListToolsWithConnectorIdResult {
    pub next_cursor: Option<String>,
    pub tools: Vec<ToolWithConnectorId>,
}

/// MCP client implemented on top of the official `rmcp` SDK.
/// https://github.com/modelcontextprotocol/rust-sdk
pub struct RmcpClient {
    state: Mutex<ClientState>,
    stdio_process: Option<StdioServerProcessHandle>,
    transport_recipe: TransportRecipe,
    initialize_context: Mutex<Option<InitializeContext>>,
    session_recovery_lock: Semaphore,
    elicitation_pause_state: ElicitationPauseState,
}

impl RmcpClient {
    pub async fn new_stdio_client(
        program: OsString,
        args: Vec<OsString>,
        env: Option<HashMap<OsString, OsString>>,
        env_vars: &[McpServerEnvVar],
        cwd: Option<String>,
        launcher: Arc<dyn StdioServerLauncher>,
    ) -> io::Result<Self> {
        let transport_recipe = TransportRecipe::Stdio {
            command: StdioServerCommand::new(program, args, env, env_vars.to_vec(), cwd),
            launcher,
        };
        let transport = Self::create_pending_transport(&transport_recipe)
            .await
            .map_err(io::Error::other)?;
        let stdio_process = match &transport {
            PendingTransport::Stdio { transport } => Some(transport.process_handle()),
            PendingTransport::StreamableHttp { .. }
            | PendingTransport::StreamableHttpWithOAuth { .. } => None,
        };

        Ok(Self {
            state: Mutex::new(ClientState::Connecting {
                transport: Some(transport),
            }),
            stdio_process,
            transport_recipe,
            initialize_context: Mutex::new(None),
            session_recovery_lock: Semaphore::new(/*permits*/ 1),
            elicitation_pause_state: ElicitationPauseState::new(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_streamable_http_client(
        server_name: &str,
        codex_home: PathBuf,
        url: &str,
        bearer_token: Option<String>,
        http_headers: Option<HashMap<String, String>>,
        env_http_headers: Option<HashMap<String, String>>,
        store_mode: OAuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
        http_client: Arc<dyn HttpClient>,
        auth_provider: Option<SharedAuthProvider>,
    ) -> Result<Self> {
        let transport_recipe = TransportRecipe::StreamableHttp {
            server_name: server_name.to_string(),
            codex_home,
            url: url.to_string(),
            bearer_token,
            http_headers,
            env_http_headers,
            store_mode,
            keyring_backend_kind,
            http_client,
            auth_provider,
        };
        let transport = Self::create_pending_transport(&transport_recipe).await?;
        Ok(Self {
            state: Mutex::new(ClientState::Connecting {
                transport: Some(transport),
            }),
            stdio_process: None,
            transport_recipe,
            initialize_context: Mutex::new(None),
            session_recovery_lock: Semaphore::new(/*permits*/ 1),
            elicitation_pause_state: ElicitationPauseState::new(),
        })
    }

    /// Perform the initialization handshake with the MCP server.
    /// https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle#initialization
    #[instrument(level = "trace", skip_all)]
    pub async fn initialize(
        &self,
        params: InitializeRequestParams,
        timeout: Option<Duration>,
        send_elicitation: SendElicitation,
        send_progress: SendProgress,
    ) -> Result<InitializeResult> {
        self.initialize_with_tool_list_changed(
            params,
            timeout,
            send_elicitation,
            send_progress,
            Box::new(|| async {}.boxed()),
        )
        .await
    }

    /// Performs initialization and forwards tool-catalog change notifications.
    #[instrument(level = "trace", skip_all)]
    pub async fn initialize_with_tool_list_changed(
        &self,
        params: InitializeRequestParams,
        timeout: Option<Duration>,
        send_elicitation: SendElicitation,
        send_progress: SendProgress,
        send_tool_list_changed: SendToolListChanged,
    ) -> Result<InitializeResult> {
        let client_service = ElicitationClientService::new(
            params.clone(),
            send_elicitation,
            send_progress,
            send_tool_list_changed,
            self.elicitation_pause_state.clone(),
        );
        let pending_transport = {
            let mut guard = self.state.lock().await;
            match &mut *guard {
                ClientState::Connecting { transport } => match transport.take() {
                    Some(transport) => transport,
                    None => return Err(anyhow!("client already initializing")),
                },
                ClientState::Ready { .. } => return Err(anyhow!("client already initialized")),
                ClientState::Closed => return Err(anyhow!("MCP client is shut down")),
            }
        };

        let (service, oauth_persistor) = self
            .connect_pending_transport_with_initialize_retries(
                pending_transport,
                client_service.clone(),
                timeout,
            )
            .await?;

        let initialize_result_rmcp = service
            .peer()
            .peer_info()
            .ok_or_else(|| anyhow!("handshake succeeded but server info was missing"))?;
        let initialize_result = initialize_result_rmcp.as_ref().clone();

        {
            let mut initialize_context = self.initialize_context.lock().await;
            *initialize_context = Some(InitializeContext {
                timeout,
                client_service,
            });
        }

        {
            let mut guard = self.state.lock().await;
            if matches!(*guard, ClientState::Closed) {
                return Err(anyhow!("MCP client is shut down"));
            }
            *guard = ClientState::Ready {
                service,
                oauth: oauth_persistor.clone(),
            };
        }

        if let Some(runtime) = oauth_persistor
            && let Err(error) = runtime.persist_if_needed().await
        {
            warn!("failed to persist OAuth tokens after initialize: {error}");
        }

        Ok(initialize_result)
    }

    pub async fn list_tools(
        &self,
        params: Option<PaginatedRequestParams>,
        timeout: Option<Duration>,
    ) -> Result<ListToolsResult> {
        self.request_tools_page(params, timeout).await
    }

    async fn request_tools_page(
        &self,
        params: Option<PaginatedRequestParams>,
        timeout: Option<Duration>,
    ) -> Result<ListToolsResult> {
        let result = self
            .run_service_operation("tools/list", timeout, move |service, tracker| {
                let request = params
                    .clone()
                    .map(ListToolsRequest::with_param)
                    .unwrap_or_default();
                async move {
                    match send_tracked_request(
                        service,
                        tracker,
                        ClientRequest::ListToolsRequest(request),
                        rmcp::service::PeerRequestOptions::no_options(),
                    )
                    .await?
                    {
                        ServerResult::ListToolsResult(result) => Ok(result),
                        _ => Err(rmcp::service::ServiceError::UnexpectedResponse),
                    }
                }
                .boxed()
            })
            .await;
        self.persist_oauth_tokens().await;
        result
    }

    #[instrument(level = "trace", skip_all)]
    pub async fn list_tools_with_connector_ids(
        &self,
        params: Option<PaginatedRequestParams>,
        timeout: Option<Duration>,
    ) -> Result<ListToolsWithConnectorIdResult> {
        let result = self.request_tools_page(params, timeout).await?;
        Ok(Self::tools_with_connector_ids(result))
    }

    fn tools_with_connector_ids(result: ListToolsResult) -> ListToolsWithConnectorIdResult {
        let tools = result
            .tools
            .into_iter()
            .map(|tool| {
                let meta = tool.meta.as_ref();
                let connector_id = Self::meta_string(meta, "connector_id");
                let connector_name = Self::meta_string(meta, "connector_name")
                    .or_else(|| Self::meta_string(meta, "connector_display_name"));
                let connector_description = Self::meta_string(meta, "connector_description")
                    .or_else(|| Self::meta_string(meta, "connectorDescription"));
                ToolWithConnectorId {
                    tool,
                    connector_id,
                    connector_name,
                    connector_description,
                }
            })
            .collect();
        ListToolsWithConnectorIdResult {
            next_cursor: result.next_cursor,
            tools,
        }
    }

    fn meta_string(meta: Option<&rmcp::model::Meta>, key: &str) -> Option<String> {
        meta.and_then(|meta| meta.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    pub async fn list_resources(
        &self,
        params: Option<PaginatedRequestParams>,
        timeout: Option<Duration>,
    ) -> Result<ListResourcesResult> {
        let result = self
            .run_service_operation("resources/list", timeout, move |service, tracker| {
                let request = params
                    .clone()
                    .map(ListResourcesRequest::with_param)
                    .unwrap_or_default();
                async move {
                    match send_tracked_request(
                        service,
                        tracker,
                        ClientRequest::ListResourcesRequest(request),
                        rmcp::service::PeerRequestOptions::no_options(),
                    )
                    .await?
                    {
                        ServerResult::ListResourcesResult(result) => Ok(result),
                        _ => Err(rmcp::service::ServiceError::UnexpectedResponse),
                    }
                }
                .boxed()
            })
            .await;
        self.persist_oauth_tokens().await;
        result
    }

    pub async fn list_resource_templates(
        &self,
        params: Option<PaginatedRequestParams>,
        timeout: Option<Duration>,
    ) -> Result<ListResourceTemplatesResult> {
        let result = self
            .run_service_operation(
                "resources/templates/list",
                timeout,
                move |service, tracker| {
                    let request = params
                        .clone()
                        .map(ListResourceTemplatesRequest::with_param)
                        .unwrap_or_default();
                    async move {
                        match send_tracked_request(
                            service,
                            tracker,
                            ClientRequest::ListResourceTemplatesRequest(request),
                            rmcp::service::PeerRequestOptions::no_options(),
                        )
                        .await?
                        {
                            ServerResult::ListResourceTemplatesResult(result) => Ok(result),
                            _ => Err(rmcp::service::ServiceError::UnexpectedResponse),
                        }
                    }
                    .boxed()
                },
            )
            .await;
        self.persist_oauth_tokens().await;
        result
    }

    pub async fn read_resource(
        &self,
        params: ReadResourceRequestParams,
        timeout: Option<Duration>,
    ) -> Result<ReadResourceResult> {
        let result = self
            .run_service_operation("resources/read", timeout, move |service, tracker| {
                let request = ReadResourceRequest::new(params.clone());
                async move {
                    match send_tracked_request(
                        service,
                        tracker,
                        ClientRequest::ReadResourceRequest(request),
                        rmcp::service::PeerRequestOptions::no_options(),
                    )
                    .await?
                    {
                        ServerResult::ReadResourceResult(result) => Ok(result),
                        _ => Err(rmcp::service::ServiceError::UnexpectedResponse),
                    }
                }
                .boxed()
            })
            .await;
        self.persist_oauth_tokens().await;
        result
    }

    pub async fn call_tool(
        &self,
        name: String,
        arguments: Option<serde_json::Value>,
        meta: Option<serde_json::Value>,
        timeout: Option<Duration>,
    ) -> Result<CallToolResult> {
        let arguments = match arguments {
            Some(Value::Object(map)) => Some(map),
            Some(other) => {
                return Err(anyhow!(
                    "MCP tool arguments must be a JSON object, got {other}"
                ));
            }
            None => None,
        };
        let meta = match meta {
            Some(Value::Object(map)) => Some(rmcp::model::Meta(map)),
            Some(other) => {
                return Err(anyhow!(
                    "MCP tool request _meta must be a JSON object, got {other}"
                ));
            }
            None => None,
        };
        let mut rmcp_params = CallToolRequestParams::new(name);
        rmcp_params.arguments = arguments;
        let result = self
            .run_service_operation("tools/call", timeout, move |service, tracker| {
                let rmcp_params = rmcp_params.clone();
                let meta = meta.clone();
                async move {
                    let mut options = rmcp::service::PeerRequestOptions::no_options();
                    options.meta = meta;
                    let result = send_tracked_request(
                        service,
                        tracker,
                        ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                            rmcp_params,
                        )),
                        options,
                    )
                    .await?;
                    match result {
                        ServerResult::CallToolResult(result) => Ok(result),
                        _ => Err(rmcp::service::ServiceError::UnexpectedResponse),
                    }
                }
                .boxed()
            })
            .await;
        self.persist_oauth_tokens().await;
        result
    }

    pub async fn send_custom_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<()> {
        let result = self
            .run_service_operation(
                "notifications/custom",
                /*timeout*/ None,
                move |service, _tracker| {
                    let params = params.clone();
                    async move {
                        service
                            .send_notification(ClientNotification::CustomNotification(
                                CustomNotification {
                                    method: method.to_string(),
                                    params,
                                    extensions: Extensions::new(),
                                },
                            ))
                            .await
                    }
                    .boxed()
                },
            )
            .await;
        self.persist_oauth_tokens().await;
        result
    }

    pub async fn send_custom_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<ServerResult> {
        let response = self
            .run_service_operation(
                "requests/custom",
                /*timeout*/ None,
                move |service, tracker| {
                    let params = params.clone();
                    async move {
                        send_tracked_request(
                            service,
                            tracker,
                            ClientRequest::CustomRequest(CustomRequest::new(method, params)),
                            rmcp::service::PeerRequestOptions::no_options(),
                        )
                        .await
                    }
                    .boxed()
                },
            )
            .await;
        self.persist_oauth_tokens().await;
        response
    }

    async fn service(&self) -> Result<Arc<RunningService<RoleClient, ElicitationClientService>>> {
        let guard = self.state.lock().await;
        match &*guard {
            ClientState::Ready { service, .. } => Ok(Arc::clone(service)),
            ClientState::Connecting { .. } => Err(anyhow!("MCP client not initialized")),
            ClientState::Closed => Err(anyhow!("MCP client is shut down")),
        }
    }

    async fn oauth_persistor(&self) -> Option<OAuthPersistor> {
        let guard = self.state.lock().await;
        match &*guard {
            ClientState::Ready {
                oauth: Some(runtime),
                ..
            } => Some(runtime.clone()),
            _ => None,
        }
    }

    /// Ask the MCP transport to close gracefully and release a reaped stdio root.
    pub async fn shutdown(&self) -> Result<()> {
        let previous_state = {
            let mut guard = self.state.lock().await;
            std::mem::replace(&mut *guard, ClientState::Closed)
        };

        if let ClientState::Ready { service, .. } = previous_state {
            match Arc::try_unwrap(service) {
                Ok(service) => {
                    service.cancel().await.map_err(|error| {
                        anyhow!("failed to close MCP transport gracefully: {error}")
                    })?;
                }
                Err(service) => {
                    service.cancellation_token().cancel();
                    while !service.is_transport_closed() {
                        time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        }

        confirm_and_reap_stdio_process(self.stdio_process.as_ref())
    }

    /// Force-close the process tree after the manager-wide grace period expires.
    pub async fn force_shutdown(&self) {
        if let Some(process) = &self.stdio_process
            && let Err(error) = process.terminate().await
        {
            warn!("failed to terminate MCP stdio server process: {error}");
        }
    }

    /// Persists tokens that RMCP's `AuthClient` refreshed during an operation.
    ///
    /// Refresh itself belongs to `AuthClient`: every request re-checks expiry
    /// under the shared authorization-manager lock, so concurrent operations
    /// near expiry share one token-endpoint call inside their timeouts.
    async fn persist_oauth_tokens(&self) {
        if let Some(runtime) = self.oauth_persistor().await
            && let Err(error) = runtime.persist_if_needed().await
        {
            warn!("failed to persist OAuth tokens: {error}");
        }
    }

    async fn create_pending_transport(
        transport_recipe: &TransportRecipe,
    ) -> Result<PendingTransport> {
        match transport_recipe {
            TransportRecipe::Stdio { command, launcher } => {
                let transport = launcher.launch(command.clone()).await?;
                Ok(PendingTransport::Stdio { transport })
            }
            TransportRecipe::StreamableHttp {
                server_name,
                codex_home,
                url,
                bearer_token,
                http_headers,
                env_http_headers,
                store_mode,
                keyring_backend_kind,
                http_client,
                auth_provider,
            } => {
                let default_headers =
                    build_default_headers(http_headers.clone(), env_http_headers.clone())?;
                let auth_provider =
                    if bearer_token.is_some() || default_headers.contains_key(AUTHORIZATION) {
                        None
                    } else {
                        auth_provider.clone()
                    };

                let initial_oauth_tokens = if bearer_token.is_none()
                    && auth_provider.is_none()
                    && !default_headers.contains_key(AUTHORIZATION)
                {
                    let codex_home = codex_home.clone();
                    let name = server_name.clone();
                    let url = url.clone();
                    let store_mode = *store_mode;
                    let keyring_backend_kind = *keyring_backend_kind;
                    match tokio::task::spawn_blocking(move || {
                        load_oauth_tokens(
                            &codex_home,
                            &name,
                            &url,
                            store_mode,
                            keyring_backend_kind,
                        )
                    })
                    .await?
                    {
                        Ok(tokens) => tokens,
                        Err(err) => {
                            warn!("failed to read tokens for server `{server_name}`: {err}");
                            None
                        }
                    }
                } else {
                    None
                };

                if let Some(initial_tokens) = initial_oauth_tokens.clone() {
                    match create_oauth_transport_and_runtime(OAuthTransportRuntimeParams {
                        server_name,
                        url,
                        codex_home: codex_home.clone(),
                        initial_tokens: initial_tokens.clone(),
                        credentials_store: *store_mode,
                        keyring_backend_kind: *keyring_backend_kind,
                        default_headers: default_headers.clone(),
                        http_client: Arc::clone(http_client),
                    })
                    .await
                    {
                        Ok((transport, oauth_persistor)) => {
                            Ok(PendingTransport::StreamableHttpWithOAuth {
                                transport,
                                oauth_persistor,
                            })
                        }
                        Err(err)
                            if err.downcast_ref::<AuthError>().is_some_and(|auth_err| {
                                matches!(auth_err, AuthError::NoAuthorizationSupport)
                            }) =>
                        {
                            let access_token = initial_tokens
                                .token_response
                                .0
                                .access_token()
                                .secret()
                                .to_string();
                            warn!(
                                "OAuth metadata discovery is unavailable for MCP server `{server_name}`; falling back to stored bearer token authentication"
                            );
                            let http_config =
                                StreamableHttpClientTransportConfig::with_uri(url.clone())
                                    .auth_header(access_token);
                            let transport = StreamableHttpClientTransport::with_client(
                                StreamableHttpClientAdapter::new(
                                    Arc::clone(http_client),
                                    default_headers,
                                    /*auth_provider*/ None,
                                ),
                                http_config,
                            );
                            Ok(PendingTransport::StreamableHttp { transport })
                        }
                        Err(err) => Err(err),
                    }
                } else {
                    let mut http_config =
                        StreamableHttpClientTransportConfig::with_uri(url.clone());
                    if let Some(bearer_token) = bearer_token.clone() {
                        http_config = http_config.auth_header(bearer_token);
                    }

                    let transport = StreamableHttpClientTransport::with_client(
                        StreamableHttpClientAdapter::new(
                            Arc::clone(http_client),
                            default_headers,
                            auth_provider,
                        ),
                        http_config,
                    );
                    Ok(PendingTransport::StreamableHttp { transport })
                }
            }
        }
    }

    async fn connect_pending_transport(
        pending_transport: PendingTransport,
        client_service: ElicitationClientService,
        timeout: Option<Duration>,
    ) -> Result<(
        Arc<RunningService<RoleClient, ElicitationClientService>>,
        Option<OAuthPersistor>,
    )> {
        let deadline = timeout.map(|duration| Instant::now() + duration);
        let (transport, oauth_persistor) = match pending_transport {
            PendingTransport::Stdio { transport } => (
                service::serve_client(client_service, transport).boxed(),
                None,
            ),
            PendingTransport::StreamableHttp { transport } => (
                service::serve_client(client_service, transport).boxed(),
                None,
            ),
            PendingTransport::StreamableHttpWithOAuth {
                transport,
                oauth_persistor,
            } => (
                service::serve_client(client_service, transport).boxed(),
                Some(oauth_persistor),
            ),
        };

        let service_result = match timeout {
            Some(duration) => match time::timeout(duration, transport).await {
                Ok(result) => {
                    result.map_err(|source| anyhow::Error::from(HandshakeError { source }))
                }
                Err(_elapsed) => Err(initialize_timeout_error(duration)),
            },
            None => transport
                .await
                .map_err(|source| anyhow::Error::from(HandshakeError { source })),
        };
        let service = match service_result {
            Ok(service) => service,
            Err(error) => {
                if let Some(runtime) = oauth_persistor {
                    let persistence = tokio::spawn(async move {
                        if let Err(persist_error) = runtime.persist_if_needed().await {
                            warn!(
                                "failed to persist OAuth tokens after failed initialize: {persist_error}"
                            );
                        }
                    });
                    // Let a retry observe refreshed credentials, but keep storage I/O
                    // within the caller's remaining startup budget. Dropping the join
                    // handle leaves the owned write running to completion.
                    if let Some(deadline) = deadline {
                        let _ = time::timeout_at(deadline, persistence).await;
                    } else {
                        let _ = persistence.await;
                    }
                }
                return Err(error);
            }
        };

        Ok((Arc::new(service), oauth_persistor))
    }

    async fn run_service_operation<T, F, Fut>(
        &self,
        label: &str,
        timeout: Option<Duration>,
        operation: F,
    ) -> Result<T>
    where
        F: Fn(Arc<RunningClientService>, OperationRequestTracker) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, rmcp::service::ServiceError>>,
    {
        let tracker = OperationRequestTracker::default();
        let mut cancellation_guard = OperationCancellationGuard::new(tracker.clone());
        let operation_future =
            self.run_service_operation_with_recovery(label, tracker.clone(), &operation);
        let result = match timeout {
            Some(duration) => match active_time_timeout(
                duration,
                self.elicitation_pause_state.subscribe(),
                operation_future,
            )
            .await
            {
                Ok(result) => result,
                Err(()) => {
                    tokio::spawn(async move {
                        tracker.cancel_current("request timeout").await;
                    });
                    Err(ClientOperationError::Timeout {
                        label: label.to_string(),
                        duration,
                    }
                    .into())
                }
            },
            None => operation_future.await,
        };
        cancellation_guard.disarm();
        result
    }

    async fn run_service_operation_with_recovery<T, F, Fut>(
        &self,
        label: &str,
        tracker: OperationRequestTracker,
        operation: &F,
    ) -> Result<T>
    where
        F: Fn(Arc<RunningClientService>, OperationRequestTracker) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, rmcp::service::ServiceError>>,
    {
        let service = self.service().await?;
        match Self::run_service_operation_with_transient_retries(
            Arc::clone(&service),
            label,
            tracker.clone(),
            operation,
        )
        .await
        {
            Ok(result) => Ok(result),
            Err(error) if Self::is_session_expired_404(&error) => {
                self.reinitialize_after_session_expiry(&service).await?;
                let recovered_service = self.service().await?;
                Self::run_service_operation_with_transient_retries(
                    recovered_service,
                    label,
                    tracker,
                    operation,
                )
                .await
                .map_err(Into::into)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn run_service_operation_with_transient_retries<T, F, Fut>(
        service: Arc<RunningClientService>,
        label: &str,
        tracker: OperationRequestTracker,
        operation: &F,
    ) -> std::result::Result<T, ClientOperationError>
    where
        F: Fn(Arc<RunningClientService>, OperationRequestTracker) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, rmcp::service::ServiceError>>,
    {
        for (attempt, retry_delay_ms) in STREAMABLE_HTTP_RETRY_DELAYS_MS
            .iter()
            .copied()
            .map(Some)
            .chain(std::iter::once(None))
            .enumerate()
        {
            match operation(Arc::clone(&service), tracker.clone())
                .await
                .map_err(ClientOperationError::from)
            {
                Ok(result) => return Ok(result),
                Err(error) if Self::is_retryable_read_operation_error(label, &error) => {
                    let Some(retry_delay_ms) = retry_delay_ms else {
                        return Err(error);
                    };
                    let delay = Duration::from_millis(retry_delay_ms);
                    warn!(
                        attempt = attempt + 1,
                        max_attempts = STREAMABLE_HTTP_RETRY_DELAYS_MS.len() + 1,
                        operation = label,
                        delay_ms = delay.as_millis(),
                        error = %error,
                        "streamable HTTP MCP read operation failed with a retryable error; retrying"
                    );
                    time::sleep(delay).await;
                }
                Err(error) => return Err(error),
            }
        }

        unreachable!("service operation retry loop should return on success or final error")
    }

    fn is_retryable_read_operation_error(label: &str, error: &ClientOperationError) -> bool {
        if !matches!(
            label,
            "tools/list" | "resources/list" | "resources/templates/list" | "resources/read"
        ) {
            return false;
        }
        let ClientOperationError::Service(rmcp::service::ServiceError::TransportSend(error)) =
            error
        else {
            return false;
        };

        error
            .error
            .downcast_ref::<StreamableHttpError<StreamableHttpClientAdapterError>>()
            .is_some_and(Self::is_retryable_streamable_http_error)
    }

    fn is_session_expired_404(error: &ClientOperationError) -> bool {
        let ClientOperationError::Service(rmcp::service::ServiceError::TransportSend(error)) =
            error
        else {
            return false;
        };

        error
            .error
            .downcast_ref::<StreamableHttpError<StreamableHttpClientAdapterError>>()
            .is_some_and(|error| {
                matches!(
                    error,
                    StreamableHttpError::Client(
                        StreamableHttpClientAdapterError::SessionExpired404
                    )
                )
            })
    }

    async fn reinitialize_after_session_expiry(
        &self,
        failed_service: &Arc<RunningService<RoleClient, ElicitationClientService>>,
    ) -> Result<()> {
        let _recovery_guard = self
            .session_recovery_lock
            .acquire()
            .await
            .map_err(|_| anyhow!("MCP client recovery semaphore closed"))?;

        {
            let guard = self.state.lock().await;
            match &*guard {
                ClientState::Ready { service, .. } if !Arc::ptr_eq(service, failed_service) => {
                    return Ok(());
                }
                ClientState::Ready { .. } => {}
                ClientState::Connecting { .. } => {
                    return Err(anyhow!("MCP client not initialized"));
                }
                ClientState::Closed => {
                    return Err(anyhow!("MCP client is shut down"));
                }
            }
        }

        let initialize_context = self
            .initialize_context
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("MCP client cannot recover before initialize succeeds"))?;
        let pending_transport = Self::create_pending_transport(&self.transport_recipe).await?;
        let (service, oauth_persistor) = self
            .connect_pending_transport_with_initialize_retries(
                pending_transport,
                initialize_context.client_service,
                initialize_context.timeout,
            )
            .await?;

        {
            let mut guard = self.state.lock().await;
            if matches!(*guard, ClientState::Closed) {
                return Err(anyhow!("MCP client is shut down"));
            }
            *guard = ClientState::Ready {
                service,
                oauth: oauth_persistor.clone(),
            };
        }

        if let Some(runtime) = oauth_persistor
            && let Err(error) = runtime.persist_if_needed().await
        {
            warn!("failed to persist OAuth tokens after session recovery: {error}");
        }

        Ok(())
    }
}

fn confirm_and_reap_stdio_process(process: Option<&StdioServerProcessHandle>) -> Result<()> {
    if let Some(process) = process {
        if !process.termination_confirmed() {
            return Err(anyhow!(
                "MCP stdio process termination was not confirmed during graceful shutdown"
            ));
        }
        process.reaped();
    }
    Ok(())
}

struct OAuthTransportRuntimeParams<'a> {
    server_name: &'a str,
    url: &'a str,
    codex_home: PathBuf,
    initial_tokens: StoredOAuthTokens,
    credentials_store: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    default_headers: HeaderMap,
    http_client: Arc<dyn HttpClient>,
}

async fn create_oauth_transport_and_runtime(
    params: OAuthTransportRuntimeParams<'_>,
) -> Result<(
    StreamableHttpClientTransport<AuthClient<StreamableHttpClientAdapter>>,
    OAuthPersistor,
)> {
    let OAuthTransportRuntimeParams {
        server_name,
        url,
        codex_home,
        initial_tokens,
        credentials_store,
        keyring_backend_kind,
        default_headers,
        http_client,
    } = params;
    let oauth_http_client = Arc::new(OAuthHttpClientAdapter::new(
        http_client.clone(),
        default_headers.clone(),
    ));
    let mut oauth_state =
        OAuthState::new_with_oauth_http_client(url.to_string(), oauth_http_client).await?;

    oauth_state
        .set_credentials(
            &initial_tokens.client_id,
            initial_tokens.token_response.0.clone(),
        )
        .await?;

    let manager = match oauth_state {
        OAuthState::Authorized(manager) => manager,
        OAuthState::Unauthorized(manager) => manager,
        _ => {
            return Err(anyhow!("unexpected OAuth state during client setup"));
        }
    };

    let auth_client = AuthClient::new(
        StreamableHttpClientAdapter::new(http_client, default_headers, /*auth_provider*/ None),
        manager,
    );
    let auth_manager = auth_client.auth_manager.clone();

    let transport = StreamableHttpClientTransport::with_client(
        auth_client,
        StreamableHttpClientTransportConfig::with_uri(url.to_string()),
    );

    let runtime = OAuthPersistor::new(
        server_name.to_string(),
        url.to_string(),
        codex_home,
        auth_manager,
        credentials_store,
        keyring_backend_kind,
        Some(initial_tokens),
    );

    Ok((transport, runtime))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::time::Duration;

    use pretty_assertions::assert_eq;
    use tokio::time;

    use super::*;

    #[tokio::test]
    async fn http_client_construction_yields_while_credential_store_is_locked() -> Result<()> {
        let codex_home = tempfile::tempdir()?;
        let lock_dir = codex_home.path().join("mcp-oauth-locks");
        std::fs::create_dir_all(&lock_dir)?;
        let lock = std::fs::File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_dir.join("file-store.lock"))?;
        lock.lock()?;
        let (release, released) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _ = released.recv_timeout(Duration::from_secs(2));
            drop(lock);
        });
        let mut client = Box::pin(RmcpClient::new_streamable_http_client(
            "locked-server",
            codex_home.path().to_path_buf(),
            "http://127.0.0.1:1/mcp",
            None,
            None,
            None,
            OAuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
            Arc::new(codex_exec_server::ReqwestHttpClient),
            None,
        ));
        let first_poll = std::future::poll_fn(|context| {
            std::task::Poll::Ready(std::future::Future::poll(client.as_mut(), context))
        })
        .await;
        let _ = release.send(());
        holder.join().expect("lock holder should exit");
        assert!(
            first_poll.is_pending(),
            "client construction must yield while credential lookup waits for the lock"
        );
        let client = client.await?;
        assert!(matches!(
            *client.state.lock().await,
            ClientState::Connecting {
                transport: Some(PendingTransport::StreamableHttp { .. })
            }
        ));
        assert!(!codex_home.path().join(".credentials.json").exists());
        Ok(())
    }

    #[test]
    fn client_operation_timeout_rounds_duration() {
        let error = ClientOperationError::Timeout {
            label: "tools/list".to_string(),
            duration: Duration::from_nanos(29_999_999_875),
        };

        assert_eq!(error.to_string(), "timed out awaiting tools/list after 30s");
    }

    #[test]
    fn shared_tool_list_projection_preserves_connector_metadata_and_cursor() {
        let mut tool = Tool::new(
            Cow::Borrowed("search"),
            Cow::Borrowed("Search"),
            Arc::new(serde_json::Map::new()),
        );
        tool.meta = Some(rmcp::model::Meta(serde_json::Map::from_iter([
            (
                "connector_id".to_string(),
                Value::String("connector-1".to_string()),
            ),
            (
                "connector_display_name".to_string(),
                Value::String("Search app".to_string()),
            ),
        ])));
        let result = RmcpClient::tools_with_connector_ids(ListToolsResult {
            tools: vec![tool],
            next_cursor: Some("next".to_string()),
            meta: None,
        });

        assert_eq!(result.next_cursor.as_deref(), Some("next"));
        assert_eq!(result.tools[0].connector_id.as_deref(), Some("connector-1"));
        assert_eq!(
            result.tools[0].connector_name.as_deref(),
            Some("Search app")
        );
    }

    #[tokio::test]
    async fn active_time_timeout_pauses_while_elicitation_is_pending() {
        let pause_state = ElicitationPauseState::new();
        let pause = pause_state.enter();
        tokio::spawn(async move {
            time::sleep(Duration::from_millis(75)).await;
            drop(pause);
        });

        let result =
            active_time_timeout(Duration::from_millis(50), pause_state.subscribe(), async {
                time::sleep(Duration::from_millis(90)).await;
                "done"
            })
            .await;

        assert_eq!(Ok("done"), result);
    }

    #[tokio::test(start_paused = true)]
    async fn active_time_budget_survives_coalesced_pause_notifications() {
        let state = ElicitationPauseState::new();
        let future = active_time_timeout(
            Duration::from_secs(10),
            state.subscribe(),
            std::future::pending::<()>(),
        );
        tokio::pin!(future);
        assert!(futures::poll!(&mut future).is_pending());
        time::advance(Duration::from_secs(6)).await;
        let first = state.enter();
        let second = state.enter();
        time::advance(Duration::from_secs(100)).await;
        drop(first);
        assert!(futures::poll!(&mut future).is_pending());
        time::advance(Duration::from_secs(100)).await;
        drop(second);
        // Coalesce resume, pause, and resume without polling the timeout.
        time::advance(Duration::from_secs(3)).await;
        let third = state.enter();
        time::advance(Duration::from_secs(100)).await;
        drop(third);
        assert!(futures::poll!(&mut future).is_pending());
        time::advance(Duration::from_secs(1)).await;
        assert_eq!(futures::poll!(&mut future), std::task::Poll::Ready(Err(())));
    }

    #[tokio::test]
    async fn graceful_shutdown_does_not_reap_unconfirmed_stdio_process() {
        let process = StdioServerProcessHandle::unconfirmed_for_test();

        let error = confirm_and_reap_stdio_process(Some(&process))
            .expect_err("unconfirmed process should require forced shutdown");
        assert!(error.to_string().contains("termination was not confirmed"));
        assert!(!process.termination_confirmed());

        process
            .terminate()
            .await
            .expect("test process termination should succeed");
        confirm_and_reap_stdio_process(Some(&process))
            .expect("confirmed process should be released");
        assert!(process.termination_confirmed());
    }
}
