//! RMCP client lifecycle for MCP server connections.
//!
//! This module owns startup of individual RMCP clients: building the transport,
//! initializing the server, listing raw tools, applying per-server tool filters,
//! and exposing cached Codex Apps tools while a client is still connecting.
//! Higher-level aggregation and resource/tool APIs live in
//! [`crate::connection_manager`].

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::codex_apps::normalize_codex_apps_callable_name;
use crate::codex_apps::normalize_codex_apps_callable_namespace;
use crate::codex_apps::normalize_codex_apps_tool_title;
use crate::codex_apps_cache::CodexAppsToolsCacheContext;
use crate::codex_apps_cache::CodexAppsToolsFetchSource;
use crate::codex_apps_cache::load_startup_cached_codex_apps_server_info;
use crate::elicitation::ElicitationRequestManager;
use crate::mcp::CODEX_APPS_MCP_SERVER_NAME;
use crate::mcp::ToolPluginProvenance;
use crate::runtime::McpRuntimeContext;
use crate::runtime::emit_duration;
use crate::server::EffectiveMcpServer;
use crate::server::McpServerLaunch;
use crate::tools::ToolFilter;
use crate::tools::ToolInfo;
use crate::tools::filter_tools;
use crate::tools::tool_with_model_visible_input_schema;
use anyhow::Result;
use anyhow::anyhow;
use arc_swap::ArcSwap;
use async_channel::Receiver;
use async_channel::Sender;
use codex_api::SharedAuthProvider;
use codex_async_utils::CancelErr;
use codex_async_utils::OrCancelExt;
use codex_config::McpServerConfig;
use codex_config::McpServerTransportConfig;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::HttpClient;
use codex_exec_server::ReqwestHttpClient;
use codex_protocol::mcp::McpServerInfo;
use codex_protocol::mcp::decode_tool_call_progress_token;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::McpStartupStatus;
use codex_protocol::protocol::McpStartupUpdateEvent;
use codex_protocol::protocol::McpToolCallProgressEvent;
use codex_rmcp_client::ExecutorStdioServerLauncher;
use codex_rmcp_client::LocalStdioServerLauncher;
use codex_rmcp_client::RmcpClient;
use codex_rmcp_client::SendProgress;
use codex_rmcp_client::SendToolListChanged;
use codex_rmcp_client::StdioServerLauncher;
use codex_rmcp_client::ToolWithConnectorId;
use codex_rmcp_client::is_authentication_required_error;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use futures::future::Shared;
use rmcp::model::ClientCapabilities;
use rmcp::model::ElicitationCapability;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParams;
use rmcp::model::JsonObject;
use rmcp::model::NumberOrString;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ProgressNotificationParam;
use rmcp::model::ProtocolVersion;
use rmcp::model::Tool as RmcpTool;
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::instrument;
use tracing::warn;

const MCP_SERVER_NAME_PATTERN: &str = "^[a-zA-Z0-9_-]+$";

/// MCP server capability indicating that Codex should include [`SandboxState`]
/// in tool-call request `_meta` under this key.
pub const MCP_SANDBOX_STATE_META_CAPABILITY: &str = "codex/sandbox-state-meta";
pub const OPENAI_FORM_CAPABILITY: &str = "openai/form";

pub(crate) const MCP_TOOLS_LIST_DURATION_METRIC: &str = "codex.mcp.tools.list.duration_ms";
pub(crate) const MCP_TOOLS_FETCH_UNCACHED_DURATION_METRIC: &str =
    "codex.mcp.tools.fetch_uncached.duration_ms";
pub(crate) const CODEX_APPS_REFRESH_DURATION_METRIC: &str = "codex.apps.refresh.duration_ms";
pub(crate) const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) const CODEX_APPS_RECONNECT_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const CODEX_APPS_RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(30);
const MAX_MCP_TOOL_LIST_PAGES: usize = 100;
const MAX_MCP_TOOL_LIST_ITEMS: usize = 10_000;

const UNTRUSTED_CONNECTOR_META_KEYS: &[&str] = &[
    "connector_id",
    "connector_name",
    "connector_display_name",
    "connector_description",
    "connectorDescription",
];

fn make_progress_sender(tx_event: Sender<Event>) -> SendProgress {
    Box::new(move |params: ProgressNotificationParam| {
        let tx_event = tx_event.clone();
        async move {
            let NumberOrString::String(token) = &params.progress_token.0 else {
                return;
            };
            let Some((turn_id, item_id)) = decode_tool_call_progress_token(token) else {
                return;
            };
            let message = params.message.unwrap_or_else(|| match params.total {
                Some(total) => format!("MCP tool progress: {}/{total}", params.progress),
                None => format!("MCP tool progress: {}", params.progress),
            });
            if let Err(err) = tx_event
                .send(Event {
                    id: turn_id,
                    msg: EventMsg::McpToolCallProgress(McpToolCallProgressEvent {
                        call_id: item_id,
                        message,
                    }),
                })
                .await
            {
                warn!("failed to forward MCP tool progress event: {err}");
            }
        }
        .boxed()
    })
}

#[derive(Clone)]
pub(crate) struct ManagedClient {
    pub(crate) client: Arc<RmcpClient>,
    pub(crate) server_info: McpServerInfo,
    pub(crate) tools: Arc<ArcSwap<Vec<ToolInfo>>>,
    pub(crate) tool_filter: ToolFilter,
    pub(crate) tool_timeout: Duration,
    pub(crate) server_instructions: Option<String>,
    pub(crate) server_supports_sandbox_state_meta_capability: bool,
    pub(crate) server_supports_resources_capability: bool,
    pub(crate) codex_apps_tools_cache_context: Option<CodexAppsToolsCacheContext>,
}

impl ManagedClient {
    fn listed_tools(&self) -> Vec<ToolInfo> {
        self.tools.load_full().as_ref().clone()
    }
}

pub(crate) type ManagedClientFuture =
    Shared<BoxFuture<'static, Result<ManagedClient, StartupOutcomeError>>>;

#[derive(Default)]
struct CodexAppsStartupReconnectState {
    current_client: Option<ManagedClient>,
    reconnect_in_flight: bool,
    consecutive_failures: u32,
    retry_not_before: Option<TokioInstant>,
}

#[derive(Clone)]
struct CodexAppsStartupStatusContext {
    submit_id: String,
    server_name: String,
    tx_event: Sender<Event>,
}

pub(crate) struct CodexAppsStartupReconnect {
    factory: Arc<dyn Fn() -> ManagedClientFuture + Send + Sync>,
    codex_apps_tools_cache_context: Option<CodexAppsToolsCacheContext>,
    tool_filter: ToolFilter,
    tool_catalog_revision: Arc<AtomicU64>,
    state: StdMutex<CodexAppsStartupReconnectState>,
    startup_status_context: Option<CodexAppsStartupStatusContext>,
}

impl CodexAppsStartupReconnect {
    pub(crate) fn new(
        factory: Arc<dyn Fn() -> ManagedClientFuture + Send + Sync>,
        codex_apps_tools_cache_context: Option<CodexAppsToolsCacheContext>,
        tool_filter: ToolFilter,
        tool_catalog_revision: Arc<AtomicU64>,
    ) -> Self {
        Self {
            factory,
            codex_apps_tools_cache_context,
            tool_filter,
            tool_catalog_revision,
            state: StdMutex::new(CodexAppsStartupReconnectState::default()),
            startup_status_context: None,
        }
    }

    fn with_startup_status_context(
        mut self,
        submit_id: String,
        server_name: String,
        tx_event: Sender<Event>,
    ) -> Self {
        self.startup_status_context = Some(CodexAppsStartupStatusContext {
            submit_id,
            server_name,
            tx_event,
        });
        self
    }

    fn current_client(&self) -> Option<ManagedClient> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current_client
            .clone()
    }

    fn reconnect_in_background(self: &Arc<Self>) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.current_client.is_some() || state.reconnect_in_flight {
                return;
            }
            if state
                .retry_not_before
                .is_some_and(|retry_not_before| TokioInstant::now() < retry_not_before)
            {
                return;
            }
            state.reconnect_in_flight = true;
        }

        let reconnect = Arc::clone(self);
        tokio::spawn(async move {
            let previously_exposed_tools = reconnect
                .codex_apps_tools_cache_context
                .as_ref()
                .and_then(CodexAppsToolsCacheContext::current_tools)
                .map(|tools| filter_tools(tools, &reconnect.tool_filter));
            let result = (reconnect.factory)().await;
            let startup_status_context = reconnect.startup_status_context.clone();
            let recovered = {
                let mut state = reconnect
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.reconnect_in_flight = false;
                match result {
                    Ok(client) => {
                        state.current_client = Some(client.clone());
                        advance_tool_catalog_revision_if_changed(
                            previously_exposed_tools.as_deref(),
                            &client.listed_tools(),
                            &reconnect.tool_catalog_revision,
                        );
                        state.consecutive_failures = 0;
                        state.retry_not_before = None;
                        true
                    }
                    Err(error) => {
                        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                        let retry_after = codex_apps_reconnect_backoff(state.consecutive_failures);
                        state.retry_not_before = Some(TokioInstant::now() + retry_after);
                        warn!(
                            error = %error,
                            retry_after_ms = retry_after.as_millis(),
                            "MCP startup reconnect failed; will retry after backoff"
                        );
                        false
                    }
                }
            };

            if recovered && let Some(context) = startup_status_context {
                let _ = context
                    .tx_event
                    .send(Event {
                        id: context.submit_id,
                        msg: EventMsg::McpStartupUpdate(McpStartupUpdateEvent {
                            server: context.server_name,
                            status: McpStartupStatus::Ready,
                        }),
                    })
                    .await;
            }
        });
    }
}

fn codex_apps_reconnect_backoff(consecutive_failures: u32) -> Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(5);
    CODEX_APPS_RECONNECT_INITIAL_BACKOFF
        .saturating_mul(1 << exponent)
        .min(CODEX_APPS_RECONNECT_MAX_BACKOFF)
}

fn reconnect_failed_startup_enabled(_server_name: &str) -> bool {
    true
}

#[derive(Clone)]
struct ManagedClientStartup {
    server_name: String,
    codex_home: PathBuf,
    server: EffectiveMcpServer,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    tx_event: Sender<Event>,
    elicitation_requests: ElicitationRequestManager,
    codex_apps_tools_cache_context: Option<CodexAppsToolsCacheContext>,
    runtime_context: McpRuntimeContext,
    runtime_auth_provider: Option<SharedAuthProvider>,
    client_elicitation_capability: ElicitationCapability,
    supports_openai_form_elicitation: bool,
    tool_catalog_revision: Arc<AtomicU64>,
    cancel_token: CancellationToken,
    startup_complete: Arc<AtomicBool>,
}

impl ManagedClientStartup {
    fn start(&self) -> ManagedClientFuture {
        self.start_inner(/*advance_cached_catalog_transition*/ true)
    }

    fn start_for_reconnect(&self) -> ManagedClientFuture {
        self.start_inner(/*advance_cached_catalog_transition*/ false)
    }

    fn start_inner(&self, advance_cached_catalog_transition: bool) -> ManagedClientFuture {
        let Self {
            server_name,
            codex_home,
            server,
            store_mode,
            keyring_backend_kind,
            tx_event,
            elicitation_requests,
            codex_apps_tools_cache_context,
            runtime_context,
            runtime_auth_provider,
            client_elicitation_capability,
            supports_openai_form_elicitation,
            tool_catalog_revision,
            cancel_token,
            startup_complete,
        } = self.clone();
        let is_codex_apps_mcp_server = server_name == CODEX_APPS_MCP_SERVER_NAME;
        let tool_filter = server
            .configured_config()
            .map(ToolFilter::from_config)
            .unwrap_or_default();
        let previously_exposed_tools = (advance_cached_catalog_transition
            && is_codex_apps_mcp_server)
            .then(|| {
                codex_apps_tools_cache_context
                    .as_ref()
                    .and_then(CodexAppsToolsCacheContext::current_tools)
                    .map(|tools| filter_tools(tools, &tool_filter))
            })
            .flatten();
        let cancel_token_for_fut = cancel_token;
        async move {
            let refresh_start = is_codex_apps_mcp_server.then(Instant::now);
            let outcome = match async {
                if let Err(error) = validate_mcp_server_name(&server_name) {
                    return Err(error.into());
                }

                let client = Arc::new(
                    make_rmcp_client(
                        &server_name,
                        codex_home,
                        server.clone(),
                        store_mode,
                        keyring_backend_kind,
                        runtime_context,
                        runtime_auth_provider,
                    )
                    .await?,
                );
                start_server_task(
                    server_name,
                    client,
                    StartServerTaskParams {
                        is_codex_apps_mcp_server,
                        startup_timeout: server
                            .configured_config()
                            .and_then(|config| config.startup_timeout_sec)
                            .or(Some(DEFAULT_STARTUP_TIMEOUT)),
                        tool_timeout: server
                            .configured_config()
                            .and_then(|config| config.tool_timeout_sec)
                            .unwrap_or(DEFAULT_TOOL_TIMEOUT),
                        tool_filter,
                        tx_event,
                        elicitation_requests,
                        codex_apps_tools_cache_context,
                        client_elicitation_capability,
                        supports_openai_form_elicitation,
                        tool_catalog_revision: Arc::clone(&tool_catalog_revision),
                    },
                )
                .await
            }
            .or_cancel(&cancel_token_for_fut)
            .await
            {
                Ok(result) => result,
                Err(CancelErr::Cancelled) => Err(StartupOutcomeError::Cancelled),
            };
            if outcome.is_ok()
                && let Some(refresh_start) = refresh_start
            {
                emit_duration(
                    CODEX_APPS_REFRESH_DURATION_METRIC,
                    refresh_start.elapsed(),
                    &[("path", "legacy"), ("trigger", "initial")],
                );
            }

            if let (Ok(client), Some(previously_exposed_tools)) =
                (&outcome, previously_exposed_tools.as_deref())
            {
                advance_tool_catalog_revision_if_changed(
                    Some(previously_exposed_tools),
                    &client.listed_tools(),
                    &tool_catalog_revision,
                );
            }

            startup_complete.store(true, Ordering::Release);
            outcome
        }
        .in_current_span()
        .boxed()
        .shared()
    }
}

#[derive(Clone)]
pub(crate) struct AsyncManagedClient {
    pub(crate) client: ManagedClientFuture,
    pub(crate) is_codex_apps_mcp_server: bool,
    pub(crate) cached_server_info: Option<McpServerInfo>,
    pub(crate) codex_apps_tools_cache_context: Option<CodexAppsToolsCacheContext>,
    pub(crate) tool_filter: ToolFilter,
    pub(crate) startup_complete: Arc<AtomicBool>,
    pub(crate) startup_reconnect: Option<Arc<CodexAppsStartupReconnect>>,
    pub(crate) tool_plugin_provenance: Arc<ToolPluginProvenance>,
    pub(crate) cancel_token: CancellationToken,
    pub(crate) manager_owners: Arc<AtomicUsize>,
}

impl AsyncManagedClient {
    // Keep this constructor flat so the startup inputs remain readable at the
    // single call site instead of introducing a one-off params wrapper.
    #[instrument(level = "trace", skip_all, fields(server_name = %server_name))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        server_name: String,
        codex_home: PathBuf,
        startup_submit_id: String,
        server: EffectiveMcpServer,
        store_mode: OAuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
        cancel_token: CancellationToken,
        tx_event: Sender<Event>,
        elicitation_requests: ElicitationRequestManager,
        codex_apps_tools_cache_context: Option<CodexAppsToolsCacheContext>,
        tool_plugin_provenance: Arc<ToolPluginProvenance>,
        runtime_context: McpRuntimeContext,
        runtime_auth_provider: Option<SharedAuthProvider>,
        client_elicitation_capability: ElicitationCapability,
        supports_openai_form_elicitation: bool,
        tool_catalog_revision: Arc<AtomicU64>,
    ) -> Self {
        let is_codex_apps_mcp_server = server_name == CODEX_APPS_MCP_SERVER_NAME;
        let reconnect_server_name = server_name.clone();
        let reconnect_tx_event = tx_event.clone();
        let tool_filter = server
            .configured_config()
            .map(ToolFilter::from_config)
            .unwrap_or_default();
        let cached_server_info = if is_codex_apps_mcp_server {
            codex_apps_tools_cache_context
                .as_ref()
                .and_then(load_startup_cached_codex_apps_server_info)
        } else {
            None
        };
        let startup_complete = Arc::new(AtomicBool::new(false));
        let startup = Arc::new(ManagedClientStartup {
            server_name,
            codex_home,
            server,
            store_mode,
            keyring_backend_kind,
            tx_event,
            elicitation_requests,
            codex_apps_tools_cache_context: codex_apps_tools_cache_context.clone(),
            runtime_context,
            runtime_auth_provider,
            client_elicitation_capability,
            supports_openai_form_elicitation,
            tool_catalog_revision: Arc::clone(&tool_catalog_revision),
            cancel_token: cancel_token.clone(),
            startup_complete: Arc::clone(&startup_complete),
        });
        let client = startup.start();
        let startup_reconnect =
            reconnect_failed_startup_enabled(&reconnect_server_name).then(|| {
                let startup = Arc::clone(&startup);
                Arc::new(
                    CodexAppsStartupReconnect::new(
                        Arc::new(move || startup.start_for_reconnect()),
                        codex_apps_tools_cache_context.clone(),
                        tool_filter.clone(),
                        tool_catalog_revision,
                    )
                    .with_startup_status_context(
                        startup_submit_id,
                        reconnect_server_name,
                        reconnect_tx_event,
                    ),
                )
            });
        if codex_apps_tools_cache_context
            .as_ref()
            .is_some_and(CodexAppsToolsCacheContext::has_current_tools)
        {
            let startup_task = client.clone();
            tokio::spawn(async move {
                let _ = startup_task.await;
            });
        }

        Self {
            client,
            is_codex_apps_mcp_server,
            cached_server_info,
            codex_apps_tools_cache_context,
            tool_filter,
            startup_complete,
            startup_reconnect,
            tool_plugin_provenance,
            cancel_token,
            manager_owners: Arc::new(AtomicUsize::new(1)),
        }
    }

    pub(crate) fn retain_for_manager(&self) {
        self.manager_owners.fetch_add(1, Ordering::AcqRel);
    }

    fn release_manager(&self) -> bool {
        self.manager_owners
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |owners| {
                (owners > 0).then_some(owners - 1)
            })
            .is_ok_and(|previous| previous == 1)
    }

    pub(crate) fn release_manager_without_shutdown(&self) {
        if self.release_manager() {
            self.cancel_token.cancel();
        }
    }

    pub(crate) async fn client(&self) -> Result<ManagedClient, StartupOutcomeError> {
        if let Some(client) = self
            .startup_reconnect
            .as_ref()
            .and_then(|reconnect| reconnect.current_client())
        {
            return Ok(client);
        }
        self.client.clone().await
    }

    pub(crate) async fn reconnect_failed_startup(&self) {
        let Some(startup_reconnect) = self.startup_reconnect.as_ref() else {
            return;
        };
        if !self.startup_complete.load(Ordering::Acquire) {
            return;
        }
        if matches!(self.client().await, Err(StartupOutcomeError::Failed { .. })) {
            startup_reconnect.reconnect_in_background();
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        if !self.release_manager() {
            return Ok(());
        }
        self.cancel_token.cancel();
        match self.client().await {
            Ok(client) => client.client.shutdown().await,
            Err(StartupOutcomeError::Cancelled) => Ok(()),
            Err(error) => {
                warn!("failed to initialize MCP client during shutdown: {error:#}");
                Ok(())
            }
        }
    }

    pub(crate) async fn force_shutdown(&self) {
        if self.manager_owners.load(Ordering::Acquire) > 0 && !self.release_manager() {
            return;
        }
        self.cancel_token.cancel();
        match self.client().await {
            Ok(client) => client.client.force_shutdown().await,
            Err(StartupOutcomeError::Cancelled) => {}
            Err(error) => {
                warn!("failed to initialize MCP client during forced shutdown: {error:#}");
            }
        }
    }

    pub(crate) fn has_cached_tools(&self) -> bool {
        self.codex_apps_tools_cache_context
            .as_ref()
            .is_some_and(CodexAppsToolsCacheContext::has_current_tools)
    }

    fn cached_tools(&self) -> Option<Vec<ToolInfo>> {
        self.codex_apps_tools_cache_context
            .as_ref()
            .and_then(CodexAppsToolsCacheContext::current_tools)
            .map(|tools| filter_tools(tools, &self.tool_filter))
    }

    pub(crate) async fn listed_tools(&self) -> Option<Vec<ToolInfo>> {
        // Keep cache payloads raw; plugin provenance is resolved per-session at read time.
        let tools = if !self.startup_complete.load(Ordering::Acquire)
            && let Some(startup_tools) = self.cached_tools()
        {
            Some(startup_tools)
        } else {
            match self.client().await {
                Ok(client) => Some(client.listed_tools()),
                Err(_) => self.cached_tools(),
            }
        }?;
        Some(if self.is_codex_apps_mcp_server {
            prepare_codex_apps_tools_for_model(tools, &self.tool_plugin_provenance)
        } else {
            prepare_regular_mcp_tools_for_model(tools, &self.tool_plugin_provenance)
        })
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum StartupOutcomeError {
    #[error("MCP startup cancelled")]
    Cancelled,
    // We can't store the original error here because anyhow::Error doesn't implement
    // `Clone`.
    #[error("MCP startup failed: {error}")]
    Failed {
        error: String,
        is_authentication_required: bool,
        is_timeout: bool,
    },
}

impl StartupOutcomeError {
    pub(crate) fn is_authentication_required(&self) -> bool {
        match self {
            Self::Cancelled => false,
            Self::Failed {
                is_authentication_required,
                ..
            } => *is_authentication_required,
        }
    }

    pub(crate) fn is_timeout(&self) -> bool {
        match self {
            Self::Cancelled => false,
            Self::Failed { is_timeout, .. } => *is_timeout,
        }
    }
}

impl From<anyhow::Error> for StartupOutcomeError {
    fn from(error: anyhow::Error) -> Self {
        let is_authentication_required = is_authentication_required_error(&error);
        let is_timeout = codex_rmcp_client::is_timeout_error(&error);
        Self::Failed {
            error: error.to_string(),
            is_authentication_required,
            is_timeout,
        }
    }
}

#[instrument(level = "trace", skip_all, fields(server_name = %server_name))]
pub(crate) async fn list_tools_for_client_uncached(
    server_name: &str,
    is_codex_apps_mcp_server: bool,
    codex_apps_refresh_trigger: &'static str,
    client: &Arc<RmcpClient>,
    timeout: Duration,
    server_instructions: Option<&str>,
) -> Result<Vec<ToolInfo>> {
    let fetch_start = Instant::now();
    let tools = collect_tool_list_pages(|params| {
        let client = Arc::clone(client);
        async move {
            client
                .list_tools_with_connector_ids(params, Some(timeout))
                .await
        }
    })
    .await?
    .into_iter()
    .map(|tool| {
        tool_info_from_listed_tool(
            server_name,
            is_codex_apps_mcp_server,
            server_instructions,
            tool,
        )
    })
    .collect();
    if is_codex_apps_mcp_server {
        emit_duration(
            MCP_TOOLS_FETCH_UNCACHED_DURATION_METRIC,
            fetch_start.elapsed(),
            &[("trigger", codex_apps_refresh_trigger)],
        );
    } else {
        emit_duration(
            MCP_TOOLS_FETCH_UNCACHED_DURATION_METRIC,
            fetch_start.elapsed(),
            &[],
        );
    }
    Ok(tools)
}

async fn collect_tool_list_pages<F, Fut>(mut fetch_page: F) -> Result<Vec<ToolWithConnectorId>>
where
    F: FnMut(Option<PaginatedRequestParams>) -> Fut,
    Fut: Future<Output = Result<codex_rmcp_client::ListToolsWithConnectorIdResult>>,
{
    let mut collected = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();

    for _page in 0..MAX_MCP_TOOL_LIST_PAGES {
        let params = cursor
            .as_ref()
            .map(|next| PaginatedRequestParams::default().with_cursor(Some(next.clone())));
        let response = fetch_page(params).await?;
        if collected.len().saturating_add(response.tools.len()) > MAX_MCP_TOOL_LIST_ITEMS {
            return Err(anyhow!("tools/list exceeded item limit"));
        }
        collected.extend(response.tools);

        match response.next_cursor {
            Some(next) => {
                if !seen_cursors.insert(next.clone()) {
                    return Err(anyhow!("tools/list returned a repeated cursor"));
                }
                cursor = Some(next);
            }
            None => return Ok(collected),
        }
    }

    Err(anyhow!("tools/list exceeded page limit"))
}

async fn discover_tools_if_supported<F, Fut>(
    supports_tools: bool,
    fetch: F,
) -> Result<Vec<ToolInfo>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<ToolInfo>>>,
{
    if supports_tools {
        fetch().await
    } else {
        Ok(Vec::new())
    }
}

/// Presents declared Codex Apps file parameters to the model as local-path inputs and adds plugin
/// names to each tool. Plugin membership is resolved by connector ID, falling back to the MCP
/// server when absent.
fn prepare_codex_apps_tools_for_model(
    mut tools: Vec<ToolInfo>,
    tool_plugin_provenance: &ToolPluginProvenance,
) -> Vec<ToolInfo> {
    for tool in &mut tools {
        tool.tool = tool_with_model_visible_input_schema(&tool.tool);
        let plugin_names = match tool.connector_id.as_deref() {
            Some(connector_id) => {
                tool_plugin_provenance.plugin_display_names_for_connector_id(connector_id)
            }
            None => tool_plugin_provenance
                .plugin_display_names_for_mcp_server_name(tool.server_name.as_str()),
        };
        add_plugin_provenance_to_tool(tool, plugin_names);
    }
    tools
}

/// Stores plugin names on the tool and appends a model-visible plugin membership note.
fn add_plugin_provenance_to_tool(tool: &mut ToolInfo, plugin_names: &[String]) {
    tool.plugin_display_names = plugin_names.to_vec();
    if plugin_names.is_empty() {
        return;
    }

    let plugin_source_note = if plugin_names.len() == 1 {
        format!("This tool is part of plugin `{}`.", plugin_names[0])
    } else {
        format!(
            "This tool is part of plugins {}.",
            plugin_names
                .iter()
                .map(|plugin_name| format!("`{plugin_name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let description = tool
        .tool
        .description
        .as_deref()
        .map(str::trim)
        .unwrap_or("");
    let annotated_description = if description.is_empty() {
        plugin_source_note
    } else if matches!(description.chars().last(), Some('.' | '!' | '?')) {
        format!("{description} {plugin_source_note}")
    } else {
        format!("{description}. {plugin_source_note}")
    };
    tool.tool.description = Some(Cow::Owned(annotated_description));
}

/// Adds server-scoped plugin names to regular MCP tools without changing their input schemas.
fn prepare_regular_mcp_tools_for_model(
    mut tools: Vec<ToolInfo>,
    tool_plugin_provenance: &ToolPluginProvenance,
) -> Vec<ToolInfo> {
    for tool in &mut tools {
        let plugin_names = tool_plugin_provenance
            .plugin_display_names_for_mcp_server_name(tool.server_name.as_str());
        add_plugin_provenance_to_tool(tool, plugin_names);
    }
    tools
}

fn tool_info_from_listed_tool(
    server_name: &str,
    is_codex_apps_mcp_server: bool,
    server_instructions: Option<&str>,
    tool: ToolWithConnectorId,
) -> ToolInfo {
    if is_codex_apps_mcp_server {
        codex_apps_tool_info_from_listed_tool(server_name, server_instructions, tool)
    } else {
        regular_mcp_tool_info_from_listed_tool(server_name, server_instructions, tool)
    }
}

/// Converts a Codex Apps tool by preserving connector fields, removing connector prefixes from
/// model-visible names and titles, and using the connector description for its tool namespace.
fn codex_apps_tool_info_from_listed_tool(
    server_name: &str,
    server_instructions: Option<&str>,
    tool: ToolWithConnectorId,
) -> ToolInfo {
    let mut tool_def = tool.tool;
    let connector_id = tool.connector_id;
    let connector_name = tool.connector_name;
    let connector_description = tool.connector_description;
    let callable_name = normalize_codex_apps_callable_name(
        &tool_def.name,
        connector_id.as_deref(),
        connector_name.as_deref(),
    );
    let callable_namespace =
        normalize_codex_apps_callable_namespace(server_name, connector_name.as_deref());
    if let Some(title) = tool_def.title.as_deref() {
        let normalized_title = normalize_codex_apps_tool_title(connector_name.as_deref(), title);
        if tool_def.title.as_deref() != Some(normalized_title.as_str()) {
            tool_def.title = Some(normalized_title);
        }
    }
    let has_connector_metadata =
        connector_id.is_some() || connector_name.is_some() || connector_description.is_some();
    let namespace_description = if has_connector_metadata {
        connector_description
    } else {
        server_instructions.map(str::to_string)
    };
    ToolInfo {
        server_name: server_name.to_owned(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name,
        callable_namespace,
        namespace_description,
        tool: tool_def,
        connector_id,
        connector_name,
        plugin_display_names: Vec::new(),
    }
}

/// Converts a regular MCP tool by removing reserved connector metadata, keeping its raw tool name,
/// and using the MCP server name and instructions for the model-visible namespace.
fn regular_mcp_tool_info_from_listed_tool(
    server_name: &str,
    server_instructions: Option<&str>,
    tool: ToolWithConnectorId,
) -> ToolInfo {
    let mut tool_def = tool.tool;
    strip_untrusted_connector_meta(&mut tool_def);
    ToolInfo {
        server_name: server_name.to_owned(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: tool_def.name.to_string(),
        callable_namespace: server_name.to_string(),
        namespace_description: server_instructions.map(str::to_string),
        tool: tool_def,
        connector_id: None,
        connector_name: None,
        plugin_display_names: Vec::new(),
    }
}

fn strip_untrusted_connector_meta(tool: &mut RmcpTool) {
    if let Some(meta) = tool.meta.as_mut() {
        meta.retain(|key, _| !is_untrusted_connector_meta_key(key));
    }
}

fn is_untrusted_connector_meta_key(key: &str) -> bool {
    UNTRUSTED_CONNECTOR_META_KEYS.contains(&key)
}

fn resolve_bearer_token(
    server_name: &str,
    bearer_token_env_var: Option<&str>,
) -> Result<Option<String>> {
    let Some(env_var) = bearer_token_env_var else {
        return Ok(None);
    };

    match env::var(env_var) {
        Ok(value) => {
            if value.is_empty() {
                Err(anyhow!(
                    "Environment variable {env_var} for MCP server '{server_name}' is empty"
                ))
            } else {
                Ok(Some(value))
            }
        }
        Err(env::VarError::NotPresent) => Err(anyhow!(
            "Environment variable {env_var} for MCP server '{server_name}' is not set"
        )),
        Err(env::VarError::NotUnicode(_)) => Err(anyhow!(
            "Environment variable {env_var} for MCP server '{server_name}' contains invalid Unicode"
        )),
    }
}

fn validate_mcp_server_name(server_name: &str) -> Result<()> {
    if server_name.is_empty()
        || !server_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(anyhow!(
            "Invalid MCP server name '{server_name}': must match pattern {MCP_SERVER_NAME_PATTERN}"
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn spawn_tool_catalog_refresh_worker(
    server_name: String,
    is_codex_apps_mcp_server: bool,
    client: Weak<RmcpClient>,
    tool_timeout: Duration,
    server_instructions: Option<String>,
    server_info: McpServerInfo,
    tool_filter: ToolFilter,
    codex_apps_tools_cache_context: Option<CodexAppsToolsCacheContext>,
    tools: Arc<ArcSwap<Vec<ToolInfo>>>,
    tool_catalog_revision: Arc<AtomicU64>,
    notifications: Receiver<()>,
) {
    tokio::spawn(async move {
        while notifications.recv().await.is_ok() {
            let Some(client) = client.upgrade() else {
                break;
            };
            let list_start = Instant::now();
            let fetch_ticket = codex_apps_tools_cache_context
                .as_ref()
                .map(|cache_context| {
                    cache_context.begin_fetch(CodexAppsToolsFetchSource::HardRefresh)
                });
            let refresh: Result<Vec<ToolInfo>> = async {
                let refreshed = list_tools_for_client_uncached(
                    &server_name,
                    is_codex_apps_mcp_server,
                    /*codex_apps_refresh_trigger*/ "notification",
                    &client,
                    tool_timeout,
                    server_instructions.as_deref(),
                )
                .await?;
                let refreshed = match (codex_apps_tools_cache_context.as_ref(), fetch_ticket) {
                    (Some(cache_context), Some(fetch_ticket)) => cache_context
                        .publish_if_newest_accepted(fetch_ticket, &server_info, refreshed),
                    (None, None) => refreshed,
                    _ => unreachable!("Codex Apps fetch ticket requires cache context"),
                };
                Ok(filter_tools(refreshed, &tool_filter))
            }
            .await;

            if is_codex_apps_mcp_server {
                emit_duration(
                    MCP_TOOLS_LIST_DURATION_METRIC,
                    list_start.elapsed(),
                    &[("cache", "miss")],
                );
            }
            if let Err(error) = commit_tool_catalog_refresh(&tools, &tool_catalog_revision, refresh)
            {
                warn!(
                    "failed to refresh tools after list-changed notification for MCP server \
                     '{server_name}': {error:#}"
                );
            }
        }
    });
}

fn commit_tool_catalog_refresh(
    tools: &Arc<ArcSwap<Vec<ToolInfo>>>,
    tool_catalog_revision: &AtomicU64,
    refresh: Result<Vec<ToolInfo>>,
) -> Result<()> {
    let refreshed = refresh?;
    if tools.load_full().as_ref() == &refreshed {
        return Ok(());
    }
    tools.store(Arc::new(refreshed));
    tool_catalog_revision.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

fn advance_tool_catalog_revision_if_changed(
    previously_exposed_tools: Option<&[ToolInfo]>,
    current_tools: &[ToolInfo],
    tool_catalog_revision: &AtomicU64,
) {
    if previously_exposed_tools.unwrap_or_default() != current_tools {
        tool_catalog_revision.fetch_add(1, Ordering::AcqRel);
    }
}

#[instrument(level = "trace", skip_all, fields(server_name = %server_name))]
async fn start_server_task(
    server_name: String,
    client: Arc<RmcpClient>,
    params: StartServerTaskParams,
) -> Result<ManagedClient, StartupOutcomeError> {
    let StartServerTaskParams {
        is_codex_apps_mcp_server,
        startup_timeout,
        tool_timeout,
        tool_filter,
        tx_event,
        elicitation_requests,
        codex_apps_tools_cache_context,
        client_elicitation_capability,
        supports_openai_form_elicitation,
        tool_catalog_revision,
    } = params;
    let params = mcp_initialize_request_params(
        client_elicitation_capability,
        supports_openai_form_elicitation,
    );

    let send_elicitation = elicitation_requests.make_sender(server_name.clone(), tx_event.clone());
    let send_progress = make_progress_sender(tx_event);
    let (tool_list_changed_tx, tool_list_changed_rx) = async_channel::bounded(1);
    let send_tool_list_changed: SendToolListChanged = Box::new(move || {
        let tool_list_changed_tx = tool_list_changed_tx.clone();
        async move {
            let _ = tool_list_changed_tx.try_send(());
        }
        .boxed()
    });

    let initialize_result = client
        .initialize_with_tool_list_changed(
            params,
            startup_timeout,
            send_elicitation,
            send_progress,
            send_tool_list_changed,
        )
        .await
        .map_err(StartupOutcomeError::from)?;

    let server_supports_sandbox_state_meta_capability = initialize_result
        .capabilities
        .experimental
        .as_ref()
        .and_then(|exp| exp.get(MCP_SANDBOX_STATE_META_CAPABILITY))
        .is_some();
    let server_supports_resources_capability = initialize_result.capabilities.resources.is_some();
    let server_supports_tools_capability = initialize_result.capabilities.tools.is_some();
    let server_info = mcp_server_info_from_implementation(initialize_result.server_info);
    let server_instructions = initialize_result.instructions;
    let tools = discover_tools_if_supported(server_supports_tools_capability, || async {
        let list_start = Instant::now();
        let fetch_ticket = codex_apps_tools_cache_context
            .as_ref()
            .map(|cache_context| cache_context.begin_fetch(CodexAppsToolsFetchSource::Startup));
        let tools = list_tools_for_client_uncached(
            &server_name,
            is_codex_apps_mcp_server,
            /*codex_apps_refresh_trigger*/ "initial",
            &client,
            tool_timeout,
            server_instructions.as_deref(),
        )
        .await?;
        let tools = match (codex_apps_tools_cache_context.as_ref(), fetch_ticket) {
            (Some(cache_context), Some(fetch_ticket)) => {
                cache_context.publish_if_newest_accepted(fetch_ticket, &server_info, tools)
            }
            (None, None) => tools,
            _ => unreachable!("Codex Apps fetch ticket requires cache context"),
        };
        if is_codex_apps_mcp_server {
            emit_duration(
                MCP_TOOLS_LIST_DURATION_METRIC,
                list_start.elapsed(),
                &[("cache", "miss")],
            );
        }
        Ok(filter_tools(tools, &tool_filter))
    })
    .await
    .map_err(StartupOutcomeError::from)?;
    let tools = Arc::new(ArcSwap::from_pointee(tools));

    if server_supports_tools_capability {
        spawn_tool_catalog_refresh_worker(
            server_name.clone(),
            is_codex_apps_mcp_server,
            Arc::downgrade(&client),
            tool_timeout,
            server_instructions.clone(),
            server_info.clone(),
            tool_filter.clone(),
            codex_apps_tools_cache_context.clone(),
            Arc::clone(&tools),
            tool_catalog_revision,
            tool_list_changed_rx,
        );
    }

    let managed = ManagedClient {
        client: Arc::clone(&client),
        server_info,
        tools,
        tool_timeout,
        tool_filter,
        server_instructions,
        server_supports_sandbox_state_meta_capability,
        server_supports_resources_capability,
        codex_apps_tools_cache_context,
    };

    Ok(managed)
}

fn mcp_initialize_request_params(
    client_elicitation_capability: ElicitationCapability,
    supports_openai_form_elicitation: bool,
) -> InitializeRequestParams {
    let mut capabilities = ClientCapabilities::default();
    capabilities.elicitation = Some(client_elicitation_capability);
    if supports_openai_form_elicitation {
        capabilities.extensions = Some(BTreeMap::from([(
            OPENAI_FORM_CAPABILITY.to_string(),
            JsonObject::new(),
        )]));
    }
    InitializeRequestParams::new(
        capabilities,
        Implementation::new("codex-mcp-client", env!("CARGO_PKG_VERSION")).with_title("Codex"),
    )
    .with_protocol_version(ProtocolVersion::V_2025_06_18)
}

fn mcp_server_info_from_implementation(server_info: Implementation) -> McpServerInfo {
    McpServerInfo {
        name: server_info.name,
        title: server_info.title,
        version: server_info.version,
        description: server_info.description,
        icons: server_info.icons.map(|icons| {
            icons
                .into_iter()
                .filter_map(|icon| serde_json::to_value(icon).ok())
                .collect()
        }),
        website_url: server_info.website_url,
    }
}

struct StartServerTaskParams {
    is_codex_apps_mcp_server: bool,
    startup_timeout: Option<Duration>, // TODO: cancel_token should handle this.
    tool_timeout: Duration,
    tool_filter: ToolFilter,
    tx_event: Sender<Event>,
    elicitation_requests: ElicitationRequestManager,
    codex_apps_tools_cache_context: Option<CodexAppsToolsCacheContext>,
    client_elicitation_capability: ElicitationCapability,
    supports_openai_form_elicitation: bool,
    tool_catalog_revision: Arc<AtomicU64>,
}

#[instrument(level = "trace", skip_all, fields(server_name = %server_name))]
async fn make_rmcp_client(
    server_name: &str,
    codex_home: PathBuf,
    server: EffectiveMcpServer,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    runtime_context: McpRuntimeContext,
    runtime_auth_provider: Option<SharedAuthProvider>,
) -> Result<RmcpClient, StartupOutcomeError> {
    let config = match server.launch() {
        McpServerLaunch::Configured(config) => config.as_ref().clone(),
    };
    let resolved_environment = runtime_context
        .resolve_server_environment(server_name, &config)
        .map_err(|err| StartupOutcomeError::from(anyhow!(err)))?;
    let is_local_environment = config.is_local_environment();
    let McpServerConfig { transport, .. } = config;

    match transport {
        McpServerTransportConfig::Stdio {
            command,
            args,
            env,
            env_vars,
            cwd,
        } => {
            let command_os: OsString = command.into();
            let args_os: Vec<OsString> = args.into_iter().map(Into::into).collect();
            let env_os = env.map(|env| {
                env.into_iter()
                    .map(|(key, value)| (key.into(), value.into()))
                    .collect::<HashMap<_, _>>()
            });
            let launcher = if is_local_environment {
                // TODO(starr): Unify local stdio MCP launch with
                // `ExecutorStdioServerLauncher` once the executor-backed path
                // preserves `LocalStdioServerLauncher` semantics.
                Arc::new(LocalStdioServerLauncher::new(
                    runtime_context.local_stdio_fallback_cwd(),
                )) as Arc<dyn StdioServerLauncher>
            } else {
                let Some(environment) = resolved_environment.as_ref() else {
                    unreachable!(
                        "non-local stdio MCP servers resolve an environment before launch"
                    );
                };
                Arc::new(ExecutorStdioServerLauncher::new(
                    environment.get_exec_backend(),
                )) as Arc<dyn StdioServerLauncher>
            };

            let cwd = cwd.map(codex_utils_path_uri::LegacyAppPathString::into_string);
            RmcpClient::new_stdio_client(command_os, args_os, env_os, &env_vars, cwd, launcher)
                .await
                .map_err(|err| StartupOutcomeError::from(anyhow!(err)))
        }
        McpServerTransportConfig::StreamableHttp {
            url,
            http_headers,
            env_http_headers,
            bearer_token_env_var,
        } => {
            let http_client = resolved_environment.as_ref().map_or_else(
                || Arc::new(ReqwestHttpClient) as Arc<dyn HttpClient>,
                |environment| environment.get_http_client(),
            );
            let resolved_bearer_token =
                match resolve_bearer_token(server_name, bearer_token_env_var.as_deref()) {
                    Ok(token) => token,
                    Err(error) => return Err(error.into()),
                };
            RmcpClient::new_streamable_http_client(
                server_name,
                codex_home,
                &url,
                resolved_bearer_token,
                http_headers,
                env_http_headers,
                store_mode,
                keyring_backend_kind,
                http_client,
                runtime_auth_provider,
            )
            .await
            .map_err(StartupOutcomeError::from)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::mcp::with_tool_call_progress_token_meta;
    use pretty_assertions::assert_eq;
    use rmcp::model::JsonObject;
    use rmcp::model::Meta;
    use rmcp::model::ProgressToken;
    use rmcp::transport::auth::AuthError;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn startup_outcome_error_identifies_authentication_required() {
        let error = anyhow::Error::new(AuthError::AuthorizationRequired)
            .context("failed to initialize MCP server");

        let error = StartupOutcomeError::from(error);

        assert!(error.is_authentication_required());
    }

    #[test]
    fn mcp_server_name_validation_enforces_the_documented_ascii_pattern() {
        assert!(validate_mcp_server_name("server_name-1").is_ok());
        assert!(validate_mcp_server_name("").is_err());
        assert!(validate_mcp_server_name("server name").is_err());
        assert!(validate_mcp_server_name("sérver").is_err());
    }

    #[test]
    fn failed_startup_reconnect_is_enabled_for_regular_mcp_servers() {
        assert!(reconnect_failed_startup_enabled("regular-server"));
        assert!(reconnect_failed_startup_enabled(CODEX_APPS_MCP_SERVER_NAME));
    }

    #[tokio::test]
    async fn progress_sender_emits_correlated_core_event() {
        let meta = with_tool_call_progress_token_meta(None, "turn-1", "item-1")
            .expect("progress metadata");
        let token = meta["progressToken"]
            .as_str()
            .expect("string progress token")
            .to_string();
        let (tx_event, rx_event) = async_channel::unbounded();
        let send_progress = make_progress_sender(tx_event);

        send_progress(ProgressNotificationParam {
            progress_token: ProgressToken(NumberOrString::String(token.into())),
            progress: 2.0,
            total: Some(5.0),
            message: Some("working".to_string()),
        })
        .await;

        let event = rx_event.recv().await.expect("forwarded progress event");
        assert_eq!(event.id, "turn-1");
        match event.msg {
            EventMsg::McpToolCallProgress(event) => {
                assert_eq!(event.call_id, "item-1");
                assert_eq!(event.message, "working");
            }
            other => panic!("expected MCP tool progress event, got {other:?}"),
        }
    }

    fn listed_test_tool(name: &str, connector_id: Option<&str>) -> ToolWithConnectorId {
        ToolWithConnectorId {
            tool: RmcpTool::new(
                name.to_string(),
                format!("test tool {name}"),
                Arc::new(JsonObject::default()),
            ),
            connector_id: connector_id.map(str::to_string),
            connector_name: None,
            connector_description: None,
        }
    }

    fn test_tool_info(name: &str) -> ToolInfo {
        tool_info_from_listed_tool(
            "test-server",
            /*is_codex_apps_mcp_server*/ false,
            /*server_instructions*/ None,
            listed_test_tool(name, None),
        )
    }

    #[tokio::test]
    async fn tool_discovery_collects_every_page_and_preserves_connector_ids() {
        let pages = Arc::new(StdMutex::new(VecDeque::from([
            codex_rmcp_client::ListToolsWithConnectorIdResult {
                next_cursor: Some("page-2".to_string()),
                tools: vec![listed_test_tool("first", Some("connector-a"))],
            },
            codex_rmcp_client::ListToolsWithConnectorIdResult {
                next_cursor: None,
                tools: vec![listed_test_tool("second", Some("connector-b"))],
            },
        ])));
        let requested_cursors = Arc::new(StdMutex::new(Vec::new()));

        let tools = collect_tool_list_pages({
            let pages = Arc::clone(&pages);
            let requested_cursors = Arc::clone(&requested_cursors);
            move |params| {
                requested_cursors
                    .lock()
                    .expect("requested cursors lock")
                    .push(params.and_then(|params| params.cursor));
                let page = pages.lock().expect("pages lock").pop_front().expect("page");
                async move { Ok(page) }
            }
        })
        .await
        .expect("paginated tool discovery");

        assert_eq!(
            tools
                .iter()
                .map(|tool| (tool.tool.name.as_ref(), tool.connector_id.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("first", Some("connector-a")),
                ("second", Some("connector-b"))
            ]
        );
        assert_eq!(
            *requested_cursors.lock().expect("requested cursors lock"),
            vec![None, Some("page-2".to_string())]
        );
    }

    #[tokio::test]
    async fn tool_discovery_rejects_a_repeated_cursor() {
        let pages = Arc::new(StdMutex::new(VecDeque::from([
            codex_rmcp_client::ListToolsWithConnectorIdResult {
                next_cursor: Some("same".to_string()),
                tools: Vec::new(),
            },
            codex_rmcp_client::ListToolsWithConnectorIdResult {
                next_cursor: Some("same".to_string()),
                tools: Vec::new(),
            },
        ])));

        let result = collect_tool_list_pages(move |_| {
            let page = pages.lock().expect("pages lock").pop_front().expect("page");
            async move { Ok(page) }
        })
        .await;
        let error = match result {
            Ok(_) => panic!("repeated cursor must fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("repeated cursor"));
    }

    #[tokio::test]
    async fn resource_only_server_skips_tool_discovery() {
        let fetch_count = AtomicUsize::new(0);

        let tools = discover_tools_if_supported(/*supports_tools*/ false, || {
            fetch_count.fetch_add(1, Ordering::Relaxed);
            async { Ok(Vec::new()) }
        })
        .await
        .expect("resource-only discovery");

        assert!(tools.is_empty());
        assert_eq!(fetch_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn catalog_refresh_atomically_adds_removes_and_changes_schemas() {
        let mut schema_v1 = test_tool_info("changed");
        schema_v1.tool.input_schema = Arc::new(
            serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": { "old": { "type": "string" } }
            }))
            .expect("schema v1"),
        );
        let tools = Arc::new(ArcSwap::from_pointee(vec![
            schema_v1,
            test_tool_info("removed"),
        ]));
        let original_snapshot = tools.load_full();
        let revision = AtomicU64::new(0);

        commit_tool_catalog_refresh(&tools, &revision, Ok(original_snapshot.as_ref().clone()))
            .expect("identical refresh");
        assert!(Arc::ptr_eq(&tools.load_full(), &original_snapshot));
        assert_eq!(revision.load(Ordering::Acquire), 0);

        let mut schema_v2 = test_tool_info("changed");
        schema_v2.tool.input_schema = Arc::new(
            serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": { "new": { "type": "integer" } }
            }))
            .expect("schema v2"),
        );
        commit_tool_catalog_refresh(
            &tools,
            &revision,
            Ok(vec![schema_v2, test_tool_info("added")]),
        )
        .expect("successful refresh");
        let refreshed_snapshot = tools.load_full();
        assert_eq!(
            refreshed_snapshot
                .iter()
                .map(|tool| tool.tool.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["changed", "added"]
        );
        assert!(
            refreshed_snapshot[0].tool.input_schema["properties"]
                .get("new")
                .is_some()
        );
        assert_eq!(revision.load(Ordering::Acquire), 1);
        assert_eq!(
            original_snapshot
                .iter()
                .map(|tool| tool.tool.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["changed", "removed"]
        );
        assert!(
            original_snapshot[0].tool.input_schema["properties"]
                .get("old")
                .is_some()
        );

        let error = commit_tool_catalog_refresh(&tools, &revision, Err(anyhow!("refresh failed")))
            .expect_err("failed refresh");
        assert_eq!(error.to_string(), "refresh failed");
        assert!(Arc::ptr_eq(&tools.load_full(), &refreshed_snapshot));
        assert_eq!(revision.load(Ordering::Acquire), 1);
    }

    #[test]
    fn cached_to_live_catalog_transition_advances_only_for_changed_tools() {
        let revision = AtomicU64::new(0);
        let cached = vec![test_tool_info("cached")];

        advance_tool_catalog_revision_if_changed(Some(&cached), &cached, &revision);
        advance_tool_catalog_revision_if_changed(None, &[test_tool_info("live")], &revision);
        assert_eq!(revision.load(Ordering::Acquire), 0);

        advance_tool_catalog_revision_if_changed(
            Some(&cached),
            &[test_tool_info("live")],
            &revision,
        );
        assert_eq!(revision.load(Ordering::Acquire), 1);
    }

    #[test]
    fn mcp_initialize_advertises_openai_form_only_when_supported() {
        let unsupported = mcp_initialize_request_params(
            ElicitationCapability::default(),
            /*supports_openai_form_elicitation*/ false,
        );
        assert_eq!(unsupported.capabilities.extensions, None);

        let supported = mcp_initialize_request_params(
            ElicitationCapability::default(),
            /*supports_openai_form_elicitation*/ true,
        );
        assert_eq!(
            supported.capabilities.extensions,
            Some(BTreeMap::from([(
                OPENAI_FORM_CAPABILITY.to_string(),
                JsonObject::new(),
            )]))
        );
    }

    fn tool_with_connector_meta() -> RmcpTool {
        RmcpTool::new(
            "capture_file_upload",
            "test tool",
            Arc::new(JsonObject::default()),
        )
        .with_meta(Meta(
            serde_json::json!({
                "connector_id": "connector_gmail",
                "connector_name": "Gmail",
                "connector_display_name": "Gmail",
                "connector_description": "Mail connector",
                "connectorDescription": "Mail connector",
                "connectorFutureField": "future connector metadata",
                "CONNECTOR_UPPERCASE": "uppercase connector metadata",
                "openai/fileParams": ["file"],
                "custom": "kept"
            })
            .as_object()
            .expect("object")
            .clone(),
        ))
    }

    #[test]
    fn custom_mcp_connector_metadata_is_stripped() {
        let mut tool = tool_with_connector_meta();

        strip_untrusted_connector_meta(&mut tool);

        let meta = tool.meta.as_ref().expect("meta");
        for key in [
            "connector_id",
            "connector_name",
            "connector_display_name",
            "connector_description",
            "connectorDescription",
        ] {
            assert!(!meta.0.contains_key(key), "{key} should be stripped");
        }
        assert!(meta.0.contains_key("connectorFutureField"));
        assert!(meta.0.contains_key("CONNECTOR_UPPERCASE"));
        assert!(meta.0.contains_key("openai/fileParams"));
        assert_eq!(
            meta.0.get("custom").and_then(|value| value.as_str()),
            Some("kept")
        );
    }

    #[test]
    fn codex_apps_connector_metadata_is_preserved() {
        let tool = tool_with_connector_meta();
        let expected_tool = tool.clone();

        let tool_info = tool_info_from_listed_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            /*is_codex_apps_mcp_server*/ true,
            /*server_instructions*/ None,
            ToolWithConnectorId {
                tool,
                connector_id: Some("connector_gmail".to_string()),
                connector_name: Some("Gmail".to_string()),
                connector_description: Some("Mail connector".to_string()),
            },
        );

        let expected = ToolInfo {
            server_name: CODEX_APPS_MCP_SERVER_NAME.to_string(),
            supports_parallel_tool_calls: false,
            server_origin: None,
            callable_name: "capture_file_upload".to_string(),
            callable_namespace: "codex_apps__gmail".to_string(),
            namespace_description: Some("Mail connector".to_string()),
            tool: expected_tool,
            connector_id: Some("connector_gmail".to_string()),
            connector_name: Some("Gmail".to_string()),
            plugin_display_names: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(tool_info).expect("serialize actual tool info"),
            serde_json::to_value(expected).expect("serialize expected tool info")
        );
    }
}
