use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
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
use rmcp::model::CustomNotification;
use rmcp::model::CustomRequest;
use rmcp::model::ElicitRequestParams;
use rmcp::model::ElicitResult;
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
use rmcp::model::RequestMetaObject;
use rmcp::model::RequestParamsMeta;
use rmcp::model::ServerResult;
use rmcp::model::Tool;
use rmcp::service::ClientCacheConfig;
use rmcp::service::RoleClient;
use rmcp::service::RunningService;
use rmcp::service::{self};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::auth::AuthClient;
use rmcp::transport::auth::AuthError;
use rmcp::transport::auth::InMemoryCredentialStore;
use rmcp::transport::auth::OAuthState;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_client::StreamableHttpError;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio::time;
use tokio::time::Instant;
use tracing::instrument;
use tracing::warn;

use crate::elicitation_client_service::ElicitationClientService;
use crate::http_client_adapter::StreamableHttpClientAdapter;
use crate::http_client_adapter::StreamableHttpClientAdapterError;
use crate::oauth::OAuthPersistor;
use crate::oauth::OAuthRequestAuthority;
use crate::oauth::StoredOAuthTokens;
use crate::oauth::load_oauth_tokens_with_authority;
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
        initialize_deadline: Arc<StdMutex<Option<Instant>>>,
    },
    StreamableHttpWithOAuth {
        transport: StreamableHttpClientTransport<AuthClient<StreamableHttpClientAdapter>>,
        oauth_persistor: OAuthPersistor,
        initialize_deadline: Arc<StdMutex<Option<Instant>>>,
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

type SharedOAuthRuntime =
    Arc<Mutex<Option<(AuthClient<StreamableHttpClientAdapter>, OAuthPersistor)>>>;

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
        active_auth_provider: Arc<StdMutex<Option<SharedAuthProvider>>>,
        oauth_runtime: SharedOAuthRuntime,
        oauth_authority: Arc<StdMutex<Option<OAuthRequestAuthority>>>,
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

// Without a parent request identity, human wait is excluded from the budgets
// of the outstanding cohort, but cannot keep unrelated work alive indefinitely.
const MAX_UNCORRELATED_ELICITATION_PAUSE: Duration = Duration::from_secs(300);

#[derive(Clone, Default)]
pub(crate) struct ElicitationPauseRegistry {
    operations: Arc<StdMutex<Vec<ElicitationPauseState>>>,
}

impl ElicitationPauseRegistry {
    fn register(&self, state: ElicitationPauseState) -> ElicitationOperationRegistration {
        self.operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(state.clone());
        ElicitationOperationRegistration {
            registry: self.clone(),
            state,
        }
    }

    pub(crate) fn enter(&self) -> Vec<ElicitationPauseGuard> {
        let operations = self
            .operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // RMCP supplies no parent-operation identity for elicitation. Its
        // progressToken identifies the incoming request, not our tools/call;
        // comparing the two directions' counters can pause the wrong call.
        // With no parent identity, pause only the operations already outstanding
        // when the dialog arrives. Each receives at most five minutes of this
        // ambiguous pause over its entire lifetime; later work is never paused.
        let bounded = operations.len() > 1;
        if bounded {
            warn!(
                max_pause_sec = MAX_UNCORRELATED_ELICITATION_PAUSE.as_secs(),
                "concurrent MCP elicitation has no parent identity; applying a bounded pause to outstanding request budgets"
            );
        }
        operations.iter().map(|state| state.enter(bounded)).collect()
    }
}

struct ElicitationOperationRegistration {
    registry: ElicitationPauseRegistry,
    state: ElicitationPauseState,
}


impl Drop for ElicitationOperationRegistration {
    fn drop(&mut self) {
        self.registry
            .operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|state| !state.timing.same_channel(&self.state.timing));
    }
}

#[derive(Clone, Copy)]
struct ElicitationTiming {
    active_count: usize,
    bounded_count: usize,
    bounded_remaining: Duration,
    ambiguous_pause: bool,
    active_elapsed: Duration,
    active_since: Instant,
}

impl ElicitationTiming {
    fn active_time(&self) -> Duration {
        self.active_elapsed
            + if self.active_count == 0 {
                if self.bounded_count == 0 {
                    self.active_since.elapsed()
                } else {
                    self.active_since
                        .elapsed()
                        .saturating_sub(self.bounded_remaining)
                }
            } else {
                Duration::ZERO
            }
    }

    fn settle(&mut self) {
        let elapsed = self.active_since.elapsed();
        self.active_elapsed = self.active_time();
        if self.active_count == 0 && self.bounded_count > 0 {
            self.bounded_remaining = self.bounded_remaining.saturating_sub(elapsed);
        }
        self.active_since = Instant::now();
    }
}

impl ElicitationPauseState {
    fn new() -> Self {
        let (timing, _rx) = watch::channel(ElicitationTiming {
            active_count: 0,
            bounded_count: 0,
            bounded_remaining: MAX_UNCORRELATED_ELICITATION_PAUSE,
            ambiguous_pause: false,
            active_elapsed: Duration::ZERO,
            active_since: Instant::now(),
        });
        Self { timing }
    }

    pub(crate) fn enter(&self, bounded: bool) -> ElicitationPauseGuard {
        let mut bounded = bounded;
        self.timing.send_modify(|timing| {
            timing.settle();
            timing.ambiguous_pause |= bounded;
            bounded = timing.ambiguous_pause;
            if bounded {
                timing.bounded_count += 1;
            } else {
                timing.active_count += 1;
            }
        });
        ElicitationPauseGuard {
            pause_state: self.clone(),
            bounded,
        }
    }

    fn subscribe(&self) -> watch::Receiver<ElicitationTiming> {
        self.timing.subscribe()
    }
}

pub(crate) struct ElicitationPauseGuard {
    pause_state: ElicitationPauseState,
    bounded: bool,
}

impl Drop for ElicitationPauseGuard {
    fn drop(&mut self) {
        self.pause_state.timing.send_modify(|timing| {
            timing.settle();
            if self.bounded {
                timing.bounded_count -= 1;
            } else {
                timing.active_count -= 1;
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
        let (active_count, active_time, bounded_remaining) = {
            let timing = pause_state.borrow_and_update();
            let bounded_remaining = if timing.bounded_count > 0 {
                timing.bounded_remaining.saturating_sub(timing.active_since.elapsed())
            } else {
                Duration::ZERO
            };
            (timing.active_count, timing.active_time(), bounded_remaining)
        };
        let remaining = duration.saturating_sub(active_time.saturating_sub(started));
        if remaining.is_zero() {
            return Err(());
        }
        tokio::select! {
            result = &mut operation => return Ok(result),
            _ = time::sleep(remaining.saturating_add(bounded_remaining)), if active_count == 0 => {},
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
    #[error(
        "MCP tools/call response unavailable; completion unknown; execution may have completed; check its state before retrying: {source}"
    )]
    CompletionUnknown {
        #[source]
        source: anyhow::Error,
    },
    #[error("timed out awaiting {label} after {duration:.0?}{}", if .label == "tools/call" {
        "; execution may have completed; check its state before retrying"
    } else {
        ""
    })]
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

#[derive(Clone)]
struct OperationRequestTracker {
    current: Arc<StdMutex<Option<TrackedRequest>>>,
    pause_state: ElicitationPauseState,
    pause_registry: ElicitationPauseRegistry,
}

impl OperationRequestTracker {
    fn new(pause_registry: ElicitationPauseRegistry) -> Self {
        Self {
            current: Arc::default(),
            pause_state: ElicitationPauseState::new(),
            pause_registry,
        }
    }
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
            CancelledNotificationParam::new(Some(id), Some(reason.to_string())),
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
    let _pause_registration = tracker.pause_registry.register(tracker.pause_state.clone());
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
    Mcp(ElicitRequestParams),
    OpenAiForm {
        meta: Option<serde_json::Value>,
        message: String,
        requested_schema: serde_json::Value,
    },
}

impl Elicitation {
    pub fn meta(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        match self {
            Self::Mcp(request) => request.meta().map(|meta| &meta.0.0),
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

impl From<ElicitResult> for ElicitationResponse {
    fn from(value: ElicitResult) -> Self {
        Self {
            action: value.action,
            content: value.content,
            meta: value.meta.map(|meta| serde_json::Value::Object(meta.0)),
        }
    }
}

impl From<ElicitationResponse> for ElicitResult {
    /// Preserves object metadata supported by RMCP. Non-object metadata is omitted;
    /// the runtime's custom response serializer supports arbitrary JSON metadata.
    fn from(value: ElicitationResponse) -> Self {
        let mut result = Self::new(value.action);
        result.content = value.content;
        result.meta = value.meta.and_then(|meta| match meta {
            serde_json::Value::Object(meta) => Some(rmcp::model::MetaObject(meta)),
            _ => None,
        });
        result
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

type RecoveryOutcome = std::result::Result<(), Arc<anyhow::Error>>;
type RecoveryObservation = watch::Receiver<Option<RecoveryOutcome>>;

#[derive(Debug)]
struct SharedRecoveryError(Arc<anyhow::Error>);

impl std::fmt::Display for SharedRecoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, formatter)
    }
}

impl std::error::Error for SharedRecoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}

impl SharedRecoveryError {
    fn into_anyhow(error: Arc<anyhow::Error>) -> anyhow::Error {
        anyhow::Error::new(Self(error))
    }
}

// A leader dropped by its caller's deadline must wake its existing waiters,
// not leave an in-flight marker behind or cause every waiter to reconnect.
struct RecoveryOwner<'a> {
    slot: &'a StdMutex<Option<RecoveryObservation>>,
    sender: watch::Sender<Option<RecoveryOutcome>>,
}

impl Drop for RecoveryOwner<'_> {
    fn drop(&mut self) {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.take();
        self.sender.send_if_modified(|outcome| {
            if outcome.is_some() {
                return false;
            }
            *outcome = Some(Err(Arc::new(anyhow!("MCP session recovery was cancelled"))));
            true
        });
    }
}

/// MCP client implemented on top of the official `rmcp` SDK.
/// https://github.com/modelcontextprotocol/rust-sdk
pub struct RmcpClient {
    state: Mutex<ClientState>,
    stdio_process: Option<StdioServerProcessHandle>,
    transport_recipe: TransportRecipe,
    initialize_context: Mutex<Option<InitializeContext>>,
    session_recovery: StdMutex<Option<RecoveryObservation>>,
    session_generation: AtomicU64,
    elicitation_pause_state: ElicitationPauseRegistry,
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
            session_recovery: StdMutex::new(None),
            session_generation: AtomicU64::new(0),
            elicitation_pause_state: ElicitationPauseRegistry::default(),
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
            active_auth_provider: Arc::default(),
            oauth_runtime: Arc::default(),
            oauth_authority: Arc::default(),
        };
        let transport = Self::create_pending_transport(&transport_recipe).await?;
        Ok(Self {
            state: Mutex::new(ClientState::Connecting {
                transport: Some(transport),
            }),
            stdio_process: None,
            transport_recipe,
            initialize_context: Mutex::new(None),
            session_recovery: StdMutex::new(None),
            session_generation: AtomicU64::new(0),
            elicitation_pause_state: ElicitationPauseRegistry::default(),
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

        let peer_info = service
            .peer()
            .peer_info()
            .ok_or_else(|| anyhow!("handshake succeeded but server info was missing"))?;
        // The `initialize` handshake always reports the server implementation.
        let server_info = peer_info
            .server_info
            .clone()
            .ok_or_else(|| anyhow!("handshake succeeded but server info was missing"))?;
        let mut initialize_result = InitializeResult::new(peer_info.capabilities.clone())
            .with_protocol_version(peer_info.protocol_version.clone())
            .with_server_info(server_info);
        initialize_result.instructions = peer_info.instructions.clone();
        initialize_result.meta = peer_info.meta.clone();

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
            self.session_generation.fetch_add(1, Ordering::AcqRel);
        }

        if let Some(runtime) = oauth_persistor
        {
            runtime.persist_if_needed().await?;
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
        self.request_tools_page_with_timing(params, timeout)
            .await
            .map(|(result, _)| result)
    }

    async fn request_tools_page_with_timing(
        &self,
        params: Option<PaginatedRequestParams>,
        timeout: Option<Duration>,
    ) -> Result<(ListToolsResult, Duration)> {
        let result = self
            .run_service_operation_with_timing("tools/list", timeout, move |service, tracker| {
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
        let persist_start = Instant::now();
        let result = self.finish_oauth_operation(result).await;
        result.map(|(result, active_time)| (result, active_time + persist_start.elapsed()))
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

    /// Reports active time so a multi-page catalog can share one timeout budget
    /// without charging interactive elicitation waits to that budget.
    pub async fn list_tools_with_connector_ids_with_timing(
        &self,
        params: Option<PaginatedRequestParams>,
        timeout: Option<Duration>,
    ) -> Result<(ListToolsWithConnectorIdResult, Duration)> {
        let (result, active_time) = self.request_tools_page_with_timing(params, timeout).await?;
        let conversion_start = Instant::now();
        let result = Self::tools_with_connector_ids(result);
        Ok((result, active_time + conversion_start.elapsed()))
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

    fn meta_string(meta: Option<&rmcp::model::MetaObject>, key: &str) -> Option<String> {
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
        self.finish_oauth_operation(result).await
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
        self.finish_oauth_operation(result).await
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
        self.finish_oauth_operation(result).await
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
            Some(Value::Object(map)) => Some(RequestMetaObject::from(map)),
            Some(other) => {
                return Err(anyhow!(
                    "MCP tool request _meta must be a JSON object, got {other}"
                ));
            }
            None => None,
        };
        let mut rmcp_params = CallToolRequestParams::new(name);
        rmcp_params.arguments = arguments;
        let attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let operation_attempted = Arc::clone(&attempted);
        let result = self
            .run_service_operation("tools/call", timeout, move |service, tracker| {
                let rmcp_params = rmcp_params.clone();
                let meta = meta.clone();
                let attempted = Arc::clone(&operation_attempted);
                async move {
                    let mut options = rmcp::service::PeerRequestOptions::no_options();
                    options.meta = meta;
                    // Once handed to the transport, a send/response error is
                    // not proof that a possibly effectful call did not run.
                    attempted.store(true, std::sync::atomic::Ordering::Release);
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
        let result = self.finish_oauth_operation(result).await;
        result.map_err(|source| {
            if attempted.load(std::sync::atomic::Ordering::Acquire) && !is_timeout_error(&source) {
                ClientOperationError::CompletionUnknown { source }.into()
            } else {
                source
            }
        })
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
        self.finish_oauth_operation(result).await
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
        self.finish_oauth_operation(response).await
    }

    async fn service(&self) -> Result<Arc<RunningService<RoleClient, ElicitationClientService>>> {
        let guard = self.state.lock().await;
        match &*guard {
            ClientState::Ready { service, .. } => Ok(Arc::clone(service)),
            ClientState::Connecting { .. } => Err(anyhow!("MCP client not initialized")),
            ClientState::Closed => Err(anyhow!("MCP client is shut down")),
        }
    }

    /// Live service health is independent of the original initialization result.
    pub async fn is_healthy(&self) -> bool {
        self.service()
            .await
            .is_ok_and(|service| !service.is_closed() && !service.is_transport_closed())
    }

    pub fn session_generation(&self) -> u64 {
        self.session_generation.load(Ordering::Acquire)
    }

    pub fn session_recovery_in_progress(&self) -> bool {
        self.session_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Read peer metadata and its generation under the same replacement lock.
    pub async fn negotiated_session(&self) -> Result<(u64, InitializeResult)> {
        let guard = self.state.lock().await;
        let ClientState::Ready { service, .. } = &*guard else {
            return Err(anyhow!("MCP client is not ready"));
        };
        let peer = service
            .peer()
            .peer_info()
            .ok_or_else(|| anyhow!("MCP peer info is missing"))?;
        let mut result = InitializeResult::new(peer.capabilities.clone())
            .with_protocol_version(peer.protocol_version.clone())
            .with_server_info(
                peer.server_info
                    .clone()
                    .ok_or_else(|| anyhow!("MCP server info is missing"))?,
            );
        result.instructions = peer.instructions.clone();
        result.meta = peer.meta.clone();
        Ok((self.session_generation(), result))
    }

    /// A healthy session may only be adopted while its loaded OAuth authority
    /// still matches the credential store's committed login/logout revision.
    pub async fn has_current_auth_authority(&self) -> bool {
        if !self.managed_auth_scope_is_current() {
            return false;
        }
        let authority = match &self.transport_recipe {
            TransportRecipe::StreamableHttp { oauth_authority, .. } => oauth_authority
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            TransportRecipe::Stdio { .. } => None,
        };
        match authority {
            Some(authority) => authority.check().await.is_ok(),
            None => true,
        }
    }

    /// Match transport auth precedence without reading/minting any credentials.
    pub fn managed_auth_scope_is_current(&self) -> bool {
        match &self.transport_recipe {
            TransportRecipe::StreamableHttp {
                active_auth_provider, ..
            } => {
                let auth = active_auth_provider.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                auth.as_ref().is_none_or(|auth| auth.validate_auth_scope().is_ok())
            }
            _ => true,
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
        // Release notification senders owned by the initialize recipe too, so
        // retired clients do not leave catalog workers waiting indefinitely.
        self.initialize_context.lock().await.take();

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
    async fn finish_oauth_operation<T>(&self, result: Result<T>) -> Result<T> {
        if let Some(runtime) = self.oauth_persistor().await {
            let durability = runtime.observe_after_operation().await;
            if let Err(error) = durability {
                return match result {
                    Ok(_) => Err(error.context(
                        "MCP operation completed, but credential durability is unconfirmed; do not replay the operation",
                    )),
                    Err(operation_error) => Err(error.context(format!(
                        "MCP operation also failed: {operation_error:#}"
                    ))),
                };
            }
        }
        result
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
                active_auth_provider,
                oauth_runtime,
                oauth_authority,
            } => {
                // Each transport/worker keeps its own deadline, even when a
                // cancelled handshake outlives its caller or a retry starts.
                let initialize_deadline = Arc::default();
                let default_headers =
                    build_default_headers(http_headers.clone(), env_http_headers.clone())?;
                let auth_provider =
                    if bearer_token.is_some() || default_headers.contains_key(AUTHORIZATION) {
                        None
                    } else {
                        auth_provider.clone()
                    };

                // Reinitialization must reuse the live credential store and
                // refresh lock; disk can lag a one-use refresh-token rotation.
                *active_auth_provider.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = auth_provider.clone();
                let mut cached_oauth = oauth_runtime.lock().await;
                if bearer_token.is_none()
                    && auth_provider.is_none()
                    && !default_headers.contains_key(AUTHORIZATION)
                    && let Some((auth_client, oauth_persistor)) = cached_oauth.as_ref()
                {
                    let mut auth_client = auth_client.clone();
                    auth_client.http_client = auth_client
                        .http_client
                        .with_initialize_deadline(Arc::clone(&initialize_deadline));
                    return Ok(PendingTransport::StreamableHttpWithOAuth {
                        transport: StreamableHttpClientTransport::with_client(
                            auth_client,
                            StreamableHttpClientTransportConfig::with_uri(url.clone()),
                        ),
                        oauth_persistor: oauth_persistor.clone(),
                        initialize_deadline,
                    });
                }
                let initial_oauth_tokens = if bearer_token.is_none()
                    && auth_provider.is_none()
                    && !default_headers.contains_key(AUTHORIZATION)
                {
                    let codex_home = codex_home.clone();
                    let name = server_name.clone();
                    let url = url.clone();
                    let store_mode = *store_mode;
                    let keyring_backend_kind = *keyring_backend_kind;
                    let (tokens, authority) = tokio::task::spawn_blocking(move || {
                        load_oauth_tokens_with_authority(
                            &codex_home,
                            &name,
                            &url,
                            store_mode,
                            keyring_backend_kind,
                        )
                    })
                    .await??;
                    *oauth_authority.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(authority);
                    tokens
                } else {
                    None
                };
                let request_authority = oauth_authority
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();

                if let Some(initial_tokens) = initial_oauth_tokens.clone() {
                    match create_oauth_transport_and_runtime(OAuthTransportRuntimeParams {
                        server_name,
                        url,
                        codex_home: codex_home.clone(),
                        initial_tokens: initial_tokens.clone(),
                        authority: request_authority.clone().ok_or_else(|| anyhow!("MCP OAuth authority was not loaded"))?,
                        credentials_store: *store_mode,
                        keyring_backend_kind: *keyring_backend_kind,
                        default_headers: default_headers.clone(),
                        http_client: Arc::clone(http_client),
                        initialize_deadline: Arc::clone(&initialize_deadline),
                    })
                    .await
                    {
                        Ok((transport, oauth_persistor, auth_client)) => {
                            *cached_oauth = Some((auth_client, oauth_persistor.clone()));
                            Ok(PendingTransport::StreamableHttpWithOAuth {
                                transport,
                                oauth_persistor,
                                initialize_deadline,
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
                                    Arc::clone(&initialize_deadline),
                                ).with_oauth_authority(request_authority),
                                http_config,
                            );
                            Ok(PendingTransport::StreamableHttp {
                                transport,
                                initialize_deadline,
                            })
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
                            Arc::clone(&initialize_deadline),
                        ).with_oauth_authority(request_authority),
                        http_config,
                    );
                    Ok(PendingTransport::StreamableHttp {
                        transport,
                        initialize_deadline,
                    })
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
        let (transport, oauth_persistor, initialize_deadline) = match pending_transport {
            PendingTransport::Stdio { transport } => (
                service::serve_client(client_service, transport).boxed(),
                None,
                None,
            ),
            PendingTransport::StreamableHttp {
                transport,
                initialize_deadline,
            } => (
                service::serve_client(client_service, transport).boxed(),
                None,
                Some(initialize_deadline),
            ),
            PendingTransport::StreamableHttpWithOAuth {
                transport,
                oauth_persistor,
                initialize_deadline,
            } => (
                service::serve_client(client_service, transport).boxed(),
                Some(oauth_persistor),
                Some(initialize_deadline),
            ),
        };
        if let Some(initialize_deadline) = &initialize_deadline {
            *initialize_deadline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = deadline;
        }

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

        // Clear only after this worker's handshake succeeds. On failure or
        // caller cancellation, a delayed OAuth retry/bootstrap POST must retain
        // this attempt's deadline, never None or a later transport's budget.
        if let Some(initialize_deadline) = initialize_deadline {
            *initialize_deadline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }

        // rmcp 3 caches responses and serves stale ones on errors by default; keep
        // every request fresh, as with rmcp 1.x.
        service
            .peer()
            .set_response_cache_config(ClientCacheConfig::disabled())
            .await;

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
        self.run_service_operation_with_timing(label, timeout, operation)
            .await
            .map(|(result, _)| result)
    }

    async fn run_service_operation_with_timing<T, F, Fut>(
        &self,
        label: &str,
        timeout: Option<Duration>,
        operation: F,
    ) -> Result<(T, Duration)>
    where
        F: Fn(Arc<RunningClientService>, OperationRequestTracker) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, rmcp::service::ServiceError>>,
    {
        let tracker = OperationRequestTracker::new(self.elicitation_pause_state.clone());
        let started = tracker.pause_state.timing.borrow().active_time();
        let mut cancellation_guard = OperationCancellationGuard::new(tracker.clone());
        let operation_future =
            self.run_service_operation_with_recovery(label, tracker.clone(), &operation);
        let result = match timeout {
            Some(duration) => match active_time_timeout(
                duration,
                tracker.pause_state.subscribe(),
                operation_future,
            )
            .await
            {
                Ok(result) => result,
                Err(()) => {
                    let tracker = tracker.clone();
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
        let active_time = tracker
            .pause_state
            .timing
            .borrow()
            .active_time()
            .saturating_sub(started);
        result.map(|result| (result, active_time))
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
                    let delay = streamable_http_retry::retry_delay(&error, retry_delay_ms);
                    warn!(
                        attempt = attempt + 1,
                        max_attempts = STREAMABLE_HTTP_RETRY_DELAYS_MS.len() + 1,
                        operation = label,
                        delay_ms = delay.as_millis(),
                        error = %error,
                        "streamable HTTP MCP read operation failed with a retryable error; retrying"
                    );
                    // The enclosing active-time timeout owns the total budget.
                    streamable_http_retry::sleep_with_retry_deadline(delay, None).await;
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
        if !Arc::ptr_eq(&self.service().await?, failed_service) {
            return Ok(());
        }
        let (mut observation, owner) = {
            let mut slot = self
                .session_recovery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(observation) = slot.as_ref() {
                (observation.clone(), None)
            } else {
                let (sender, observation) = watch::channel(None);
                *slot = Some(observation.clone());
                (
                    observation,
                    Some(RecoveryOwner {
                        slot: &self.session_recovery,
                        sender,
                    }),
                )
            }
        };
        if let Some(owner) = owner {
            let outcome = self
                .reinitialize_session(failed_service)
                .await
                .map_err(Arc::new);
            owner.sender.send_replace(Some(outcome.clone()));
            drop(owner);
            return outcome.map_err(SharedRecoveryError::into_anyhow);
        }
        loop {
            if let Some(outcome) = observation.borrow_and_update().clone() {
                return outcome.map_err(SharedRecoveryError::into_anyhow);
            }
            observation
                .changed()
                .await
                .map_err(|_| anyhow!("MCP recovery owner disappeared"))?;
        }
    }

    async fn reinitialize_session(
        &self,
        failed_service: &Arc<RunningService<RoleClient, ElicitationClientService>>,
    ) -> Result<()> {
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
        let client_service = initialize_context.client_service.clone();
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
            self.session_generation.fetch_add(1, Ordering::AcqRel);
        }

        // Reinitialization is a new negotiated catalog, even without a server
        // tools/list_changed notification. The existing coalescing owner fills it.
        client_service.notify_session_changed().await;

        if let Some(runtime) = oauth_persistor
        {
            runtime.persist_if_needed().await?;
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
    authority: OAuthRequestAuthority,
    credentials_store: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    default_headers: HeaderMap,
    http_client: Arc<dyn HttpClient>,
    initialize_deadline: Arc<StdMutex<Option<Instant>>>,
}

async fn create_oauth_transport_and_runtime(
    params: OAuthTransportRuntimeParams<'_>,
) -> Result<(
    StreamableHttpClientTransport<AuthClient<StreamableHttpClientAdapter>>,
    OAuthPersistor,
    AuthClient<StreamableHttpClientAdapter>,
)> {
    let OAuthTransportRuntimeParams {
        server_name,
        url,
        codex_home,
        initial_tokens,
        authority,
        credentials_store,
        keyring_backend_kind,
        default_headers,
        http_client,
        initialize_deadline,
    } = params;
    let oauth_http_client = Arc::new(
        OAuthHttpClientAdapter::new(http_client.clone(), url, default_headers.clone())
            .map_err(anyhow::Error::from_boxed)?
            .with_authority(authority.clone()),
    );
    let mut oauth_state =
        OAuthState::new_with_oauth_http_client(url.to_string(), oauth_http_client).await?;

    let credential_store = InMemoryCredentialStore::new();
    let OAuthState::Unauthorized(manager) = &mut oauth_state else {
        return Err(anyhow!("unexpected OAuth state before client setup"));
    };
    // Keep accepting authorization-server metadata that omits `issuer`, as rmcp 1.x did.
    manager.set_allow_missing_issuer(true);
    manager.set_credential_store(credential_store.clone());

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
        StreamableHttpClientAdapter::new(
            http_client,
            default_headers,
            /*auth_provider*/ None,
            initialize_deadline,
        ).with_oauth_authority(Some(authority.clone())),
        manager,
    );

    let transport = StreamableHttpClientTransport::with_client(
        auth_client.clone(),
        StreamableHttpClientTransportConfig::with_uri(url.to_string()),
    );

    let runtime = OAuthPersistor::new(
        server_name.to_string(),
        url.to_string(),
        codex_home,
        credential_store,
        credentials_store,
        keyring_backend_kind,
        Some(initial_tokens),
        authority,
    );

    Ok((transport, runtime, auth_client))
}

