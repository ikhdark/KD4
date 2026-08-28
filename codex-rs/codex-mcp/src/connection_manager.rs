//! Aggregates MCP server connections for Codex.
//!
//! [`McpConnectionManager`] owns the set of running async RMCP clients keyed by
//! MCP server name. It coordinates startup status events, keeps server origin
//! metadata, aggregates tools/resources/templates across servers, routes tool
//! calls to the right client, and exposes the public manager API used by
//! `codex-core`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Display;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::McpAuthStatusEntry;
use crate::codex_apps_cache::CodexAppsToolsCache;
use crate::codex_apps_cache::CodexAppsToolsCacheKey;
use crate::codex_apps_cache::CodexAppsToolsFetchSource;
use crate::elicitation::ElicitationRequestManager;
use crate::elicitation::ElicitationRequestRouter;
use crate::elicitation::ElicitationReviewerHandle;
use crate::mcp::CODEX_APPS_MCP_SERVER_NAME;
use crate::mcp::ToolPluginProvenance;
use crate::rmcp_client::AsyncManagedClient;
use crate::rmcp_client::CODEX_APPS_REFRESH_DURATION_METRIC;
use crate::rmcp_client::DEFAULT_STARTUP_TIMEOUT;
use crate::rmcp_client::MCP_TOOLS_LIST_DURATION_METRIC;
use crate::rmcp_client::ManagedClient;
use crate::rmcp_client::StartupOutcomeError;
use crate::rmcp_client::list_tools_for_client_uncached;
use crate::runtime::McpRuntimeContext;
use crate::runtime::emit_duration;
use crate::server::EffectiveMcpServer;
use crate::server::McpServerMetadata;
use crate::tools::ToolInfo;
use crate::tools::filter_tools;
use crate::tools::normalize_tools_for_model_with_prefix;
use crate::tools::tool_with_model_visible_input_schema;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use async_channel::Sender;
use codex_api::SharedAuthProvider;
use codex_config::Constrained;
use codex_config::McpServerAuth;
use codex_config::McpServerTransportConfig;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::mcp::McpServerInfo;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::McpStartupCompleteEvent;
use codex_protocol::protocol::McpStartupFailure;
use codex_protocol::protocol::McpStartupFailureReason;
use codex_protocol::protocol::McpStartupStatus;
use codex_protocol::protocol::McpStartupUpdateEvent;
use codex_rmcp_client::ElicitationResponse;
use codex_rmcp_client::McpAuthState;
use codex_rmcp_client::McpLoginRequirement;
use rmcp::model::ElicitationCapability;
use rmcp::model::ListResourceTemplatesResult;
use rmcp::model::ListResourcesResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ReadResourceRequestParams;
use rmcp::model::ReadResourceResult;
use rmcp::model::RequestId;
use rmcp::model::Resource;
use rmcp::model::ResourceTemplate;
use serde_json::Value as JsonValue;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::info_span;
use tracing::instrument;
use tracing::trace;
use tracing::trace_span;
use tracing::warn;

const MCP_UI_META_KEY: &str = "ui";
const MCP_UI_VISIBILITY_META_KEY: &str = "visibility";
const MCP_UI_MODEL_VISIBILITY: &str = "model";
const MAX_MCP_SERVER_COLLECTION_ERROR_CHARS: usize = 240;
const MAX_MCP_COLLECTION_PAGES: usize = 100;
const MAX_MCP_COLLECTION_ITEMS: usize = 10_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerCollectionError {
    pub server: String,
    pub message: String,
}

impl McpServerCollectionError {
    fn new(server: String, message: impl Display) -> Self {
        Self {
            server,
            message: sanitize_server_collection_error(message),
        }
    }
}

#[derive(Debug)]
pub struct McpServerCollection<T> {
    pub results: HashMap<String, T>,
    pub errors: Vec<McpServerCollectionError>,
}

impl<T> Default for McpServerCollection<T> {
    fn default() -> Self {
        Self {
            results: HashMap::new(),
            errors: Vec::new(),
        }
    }
}

fn sanitize_server_collection_error(message: impl Display) -> String {
    let normalized = message
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = normalized.chars();
    let bounded: String = chars
        .by_ref()
        .take(MAX_MCP_SERVER_COLLECTION_ERROR_CHARS)
        .collect();
    if bounded.is_empty() {
        return "request failed".to_string();
    }
    if chars.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

/// Returns whether a tool may be included in model-facing tool declarations.
///
/// Tools without visibility metadata remain visible.
/// Tools with visibility metadata are hidden unless they explicitly include `model`.
///
/// <https://github.com/modelcontextprotocol/ext-apps/blob/main/specification/2026-01-26/apps.mdx#resource-discovery>
pub fn tool_is_model_visible(tool: &ToolInfo) -> bool {
    let Some(visibility) = tool
        .tool
        .meta
        .as_deref()
        .and_then(|meta| meta.get(MCP_UI_META_KEY))
        .and_then(JsonValue::as_object)
        .and_then(|ui| ui.get(MCP_UI_VISIBILITY_META_KEY))
        .and_then(JsonValue::as_array)
    else {
        return true;
    };

    visibility
        .iter()
        .any(|target| target.as_str() == Some(MCP_UI_MODEL_VISIBILITY))
}

/// A thin wrapper around a set of running [`RmcpClient`] instances.
pub struct McpConnectionManager {
    clients: HashMap<String, AsyncManagedClient>,
    server_definitions: HashMap<String, EffectiveMcpServer>,
    server_metadata: HashMap<String, McpServerMetadata>,
    required_servers: Vec<String>,
    tool_plugin_provenance: Arc<ToolPluginProvenance>,
    tool_catalog_revision: Arc<AtomicU64>,
    tool_catalog_cache: StdMutex<Option<CachedToolCatalog>>,
    prefix_mcp_tool_names: bool,
    elicitation_requests: ElicitationRequestManager,
    client_reuse_context: ClientReuseContext,
    shutdown_started: AtomicBool,
}

#[derive(Clone)]
struct ClientReuseContext {
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    runtime_context: McpRuntimeContext,
    codex_home: PathBuf,
    codex_apps_tools_cache_key: CodexAppsToolsCacheKey,
    client_elicitation_capability: ElicitationCapability,
    supports_openai_form_elicitation: bool,
}

impl ClientReuseContext {
    fn is_compatible_with(&self, other: &Self) -> bool {
        self.store_mode == other.store_mode
            && self.keyring_backend_kind == other.keyring_backend_kind
            && self
                .runtime_context
                .is_compatible_with(&other.runtime_context)
            && self.codex_home == other.codex_home
            && self.codex_apps_tools_cache_key == other.codex_apps_tools_cache_key
            && self.client_elicitation_capability == other.client_elicitation_capability
            && self.supports_openai_form_elicitation == other.supports_openai_form_elicitation
    }
}

struct CachedToolCatalog {
    revision: u64,
    tools: Arc<Vec<ToolInfo>>,
}

async fn shutdown_clients_with_deadline<T, G, GFut, F, FFut>(
    clients: Vec<T>,
    grace_period: Duration,
    graceful_shutdown: G,
    force_shutdown: F,
) where
    T: Clone + Send + 'static,
    G: Fn(T) -> GFut,
    GFut: Future<Output = Result<()>> + Send + 'static,
    F: Fn(T) -> FFut,
    FFut: Future<Output = ()> + Send + 'static,
{
    let mut shutdowns = JoinSet::new();
    let mut graceful_clients = HashMap::new();
    for client in &clients {
        let abort_handle = shutdowns.spawn(graceful_shutdown(client.clone()));
        graceful_clients.insert(abort_handle.id(), client.clone());
    }

    let mut failed_clients = Vec::new();
    let graceful_timed_out = tokio::time::timeout(grace_period, async {
        while let Some(result) = shutdowns.join_next_with_id().await {
            match result {
                Ok((id, result)) => {
                    if let Some(client) = graceful_clients.remove(&id)
                        && let Err(error) = result
                    {
                        warn!("MCP client graceful shutdown failed: {error:#}");
                        failed_clients.push(client);
                    }
                }
                Err(error) => {
                    warn!("MCP client graceful shutdown failed: {error}");
                    if let Some(client) = graceful_clients.remove(&error.id()) {
                        failed_clients.push(client);
                    }
                }
            }
        }
    })
    .await
    .is_err();

    if graceful_timed_out {
        shutdowns.abort_all();
        failed_clients.extend(graceful_clients.into_values());
    }

    if !failed_clients.is_empty() {
        let mut forced = JoinSet::new();
        for client in failed_clients {
            forced.spawn(force_shutdown(client));
        }
        let forced_result = tokio::time::timeout(grace_period, async {
            while let Some(result) = forced.join_next().await {
                if let Err(error) = result {
                    warn!("MCP client forced shutdown failed: {error}");
                }
            }
        })
        .await;
        if forced_result.is_err() {
            warn!("MCP client forced shutdown exceeded its deadline");
            forced.abort_all();
        }
    }
}

impl McpConnectionManager {
    async fn reusable_client(
        &self,
        server_name: &str,
        server: &EffectiveMcpServer,
    ) -> Option<&AsyncManagedClient> {
        let previous_server = self.server_definitions.get(server_name)?;
        let client = self.clients.get(server_name)?;
        let uses_chatgpt_auth = server
            .configured_config()
            .is_some_and(|config| matches!(&config.auth, McpServerAuth::ChatGpt));
        if previous_server != server
            || uses_chatgpt_auth
            || !client.startup_complete.load(Ordering::Acquire)
            || client.client().await.is_err()
        {
            return None;
        }
        Some(client)
    }

    /// Returns the coarse negotiated resources capability for fully started servers.
    pub async fn has_ready_server_with_resources(&self) -> bool {
        for client in self.clients.values() {
            if !client.startup_complete.load(Ordering::Acquire) {
                continue;
            }
            if let Ok(managed_client) = client.client().await
                && managed_client.server_supports_resources_capability
            {
                return true;
            }
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        mcp_servers: &HashMap<String, EffectiveMcpServer>,
        store_mode: OAuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
        auth_entries: HashMap<String, McpAuthStatusEntry>,
        approval_policy: &Constrained<AskForApproval>,
        submit_id: String,
        tx_event: Sender<Event>,
        startup_cancellation_token: CancellationToken,
        initial_permission_profile: PermissionProfile,
        runtime_context: McpRuntimeContext,
        codex_home: PathBuf,
        codex_apps_tools_cache: CodexAppsToolsCache,
        codex_apps_tools_cache_key: CodexAppsToolsCacheKey,
        prefix_mcp_tool_names: bool,
        client_elicitation_capability: ElicitationCapability,
        supports_openai_form_elicitation: bool,
        tool_plugin_provenance: ToolPluginProvenance,
        auth: Option<&CodexAuth>,
        codex_apps_auth_manager: Option<Arc<AuthManager>>,
        elicitation_reviewer: Option<ElicitationReviewerHandle>,
        elicitation_lifecycle: Option<crate::ElicitationLifecycle>,
        elicitation_router: ElicitationRequestRouter,
        previous_manager: Option<&McpConnectionManager>,
    ) -> Self {
        let mut required_servers = mcp_servers
            .iter()
            .filter(|(_, server)| server.enabled() && server.required())
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        required_servers.sort();
        let mut clients = HashMap::new();
        let mut server_metadata = HashMap::new();
        let mut reused_server_names = Vec::new();
        let mut join_set = JoinSet::new();
        let tool_plugin_provenance = Arc::new(tool_plugin_provenance);
        let client_reuse_context = ClientReuseContext {
            store_mode,
            keyring_backend_kind,
            runtime_context: runtime_context.clone(),
            codex_home: codex_home.clone(),
            codex_apps_tools_cache_key: codex_apps_tools_cache_key.clone(),
            client_elicitation_capability: client_elicitation_capability.clone(),
            supports_openai_form_elicitation,
        };
        let reusable_previous_manager = previous_manager.filter(|previous| {
            previous
                .client_reuse_context
                .is_compatible_with(&client_reuse_context)
                && previous.tool_plugin_provenance.as_ref() == tool_plugin_provenance.as_ref()
        });
        let elicitation_requests = if let Some(previous) = reusable_previous_manager {
            let requests = previous.elicitation_requests.clone();
            if let Ok(mut policy) = requests.approval_policy.lock() {
                *policy = approval_policy.value();
            }
            if let Ok(mut profile) = requests.permission_profile.lock() {
                *profile = initial_permission_profile;
            }
            requests
        } else {
            ElicitationRequestManager::new(
                approval_policy.value(),
                initial_permission_profile,
                elicitation_reviewer,
                elicitation_lifecycle,
                elicitation_router,
            )
        };
        let tool_catalog_revision = if let Some(previous) = reusable_previous_manager {
            let revision = Arc::clone(&previous.tool_catalog_revision);
            revision.fetch_add(1, Ordering::AcqRel);
            revision
        } else {
            Arc::new(AtomicU64::new(0))
        };
        let startup_submit_id = submit_id.clone();
        let static_chatgpt_auth_provider = auth
            .filter(|auth| auth.uses_codex_backend())
            .map(codex_model_provider::auth_provider_from_auth);
        let codex_apps_auth_provider = codex_apps_auth_manager.and_then(|auth_manager| {
            auth.filter(|auth| auth.uses_codex_backend()).map(|auth| {
                codex_model_provider::auth_provider_from_auth_manager(auth_manager, auth)
            })
        });
        let mcp_servers = mcp_servers.clone();
        let server_definitions = mcp_servers
            .iter()
            .filter(|(_, server)| server.enabled())
            .map(|(name, server)| (name.clone(), server.clone()))
            .collect();
        for (server_name, server) in mcp_servers
            .into_iter()
            .filter(|(_, server)| server.enabled())
        {
            server_metadata.insert(server_name.clone(), McpServerMetadata::from(&server));
            let reusable_client = match reusable_previous_manager {
                Some(previous) => previous.reusable_client(&server_name, &server).await,
                None => None,
            };
            if let Some(client) = reusable_client {
                client.retain_for_manager();
                clients.insert(server_name.clone(), client.clone());
                reused_server_names.push(server_name);
                continue;
            }
            let cancel_token = startup_cancellation_token.child_token();
            let _ = emit_update(
                startup_submit_id.as_str(),
                &tx_event,
                McpStartupUpdateEvent {
                    server: server_name.clone(),
                    status: McpStartupStatus::Starting,
                },
            )
            .await;
            let configured_config = server.configured_config();
            // For built-in Codex Apps, `CODEX_CONNECTORS_TOKEN` is a debug
            // override: it supplies runtime auth but bypasses the shared tools
            // cache.
            let uses_env_bearer_token =
                configured_config.is_some_and(|config| match &config.transport {
                    McpServerTransportConfig::StreamableHttp {
                        bearer_token_env_var,
                        ..
                    } => bearer_token_env_var.is_some(),
                    McpServerTransportConfig::Stdio { .. } => false,
                });
            let shares_codex_apps_tools_cache =
                should_share_codex_apps_tools_cache(&server_name, uses_env_bearer_token);
            let codex_apps_tools_cache_context = shares_codex_apps_tools_cache.then(|| {
                codex_apps_tools_cache
                    .context(codex_home.clone(), codex_apps_tools_cache_key.clone())
            });
            // The reserved Codex Apps registration follows the shared
            // AuthManager across refreshes. In the hosted-plugin path, this
            // is the ChatGPT /ps/mcp connection. User-configured MCP
            // registrations keep their existing configured auth path.
            let chatgpt_auth_provider = if server_name == CODEX_APPS_MCP_SERVER_NAME {
                codex_apps_auth_provider
                    .clone()
                    .or_else(|| static_chatgpt_auth_provider.clone())
            } else {
                static_chatgpt_auth_provider.clone()
            };
            // If Codex Apps has an env bearer token, that is its auth path. Do
            // not also attach the ambient CodexAuth provider.
            let runtime_auth_provider =
                if server_name == CODEX_APPS_MCP_SERVER_NAME && uses_env_bearer_token {
                    None
                } else {
                    chatgpt_auth_provider_for_server(&server, chatgpt_auth_provider)
                };
            let async_managed_client = AsyncManagedClient::new(
                server_name.clone(),
                codex_home.clone(),
                startup_submit_id.clone(),
                server,
                store_mode,
                keyring_backend_kind,
                cancel_token.clone(),
                tx_event.clone(),
                elicitation_requests.clone(),
                codex_apps_tools_cache_context,
                Arc::clone(&tool_plugin_provenance),
                runtime_context.clone(),
                runtime_auth_provider,
                client_elicitation_capability.clone(),
                supports_openai_form_elicitation,
                Arc::clone(&tool_catalog_revision),
            );
            clients.insert(server_name.clone(), async_managed_client.clone());
            let tx_event = tx_event.clone();
            let submit_id = startup_submit_id.clone();
            let auth_entry = auth_entries.get(&server_name).cloned();
            join_set.spawn(async move {
                let mut outcome = async_managed_client.client().await;
                if cancel_token.is_cancelled() {
                    outcome = Err(StartupOutcomeError::Cancelled);
                }
                let status = match &outcome {
                    Ok(_) => McpStartupStatus::Ready,
                    Err(StartupOutcomeError::Cancelled) => McpStartupStatus::Cancelled,
                    Err(error) => {
                        let reason = mcp_startup_failure_reason(auth_entry.as_ref(), error);
                        let error_str = mcp_init_error_display(
                            server_name.as_str(),
                            auth_entry.as_ref(),
                            error,
                        );
                        McpStartupStatus::Failed {
                            error: error_str,
                            reason,
                        }
                    }
                };

                let _ = emit_update(
                    submit_id.as_str(),
                    &tx_event,
                    McpStartupUpdateEvent {
                        server: server_name.clone(),
                        status,
                    },
                )
                .await;

                if matches!(&outcome, Err(StartupOutcomeError::Failed { .. })) {
                    async_managed_client.reconnect_failed_startup().await;
                }

                (server_name, outcome)
            });
        }
        let manager = Self {
            clients,
            server_definitions,
            server_metadata,
            required_servers,
            tool_plugin_provenance,
            tool_catalog_revision,
            tool_catalog_cache: StdMutex::new(None),
            prefix_mcp_tool_names,
            elicitation_requests: elicitation_requests.clone(),
            client_reuse_context,
            shutdown_started: AtomicBool::new(false),
        };
        tokio::spawn(async move {
            let outcomes = join_set.join_all().await;
            let mut summary = McpStartupCompleteEvent::default();
            summary.ready.extend(reused_server_names);
            for (server_name, outcome) in outcomes {
                match outcome {
                    Ok(_) => summary.ready.push(server_name),
                    Err(StartupOutcomeError::Cancelled) => summary.cancelled.push(server_name),
                    Err(StartupOutcomeError::Failed { error, .. }) => {
                        summary.failed.push(McpStartupFailure {
                            server: server_name,
                            error,
                        })
                    }
                }
            }
            let _ = tx_event
                .send(Event {
                    id: startup_submit_id,
                    msg: EventMsg::McpStartupComplete(summary),
                })
                .await;
        });
        manager
    }

    /// Waits for every required server and reports their startup failures together.
    ///
    /// Callers must make the manager reachable to request handlers before awaiting this method,
    /// because server initialization may require client elicitation.
    pub async fn validate_required_servers(&self) -> Result<()> {
        let failures = async {
            let mut failures = Vec::new();
            for server_name in &self.required_servers {
                let Some(async_managed_client) = self.clients.get(server_name).cloned() else {
                    failures.push(McpStartupFailure {
                        server: server_name.clone(),
                        error: format!("required MCP server `{server_name}` was not initialized"),
                    });
                    continue;
                };

                match async_managed_client.client().await {
                    Ok(_) => {}
                    Err(error) => failures.push(McpStartupFailure {
                        server: server_name.clone(),
                        error: startup_outcome_error_message(error),
                    }),
                }
            }
            failures
        }
        .instrument(info_span!(
            "session_init.required_mcp_wait",
            otel.name = "session_init.required_mcp_wait",
            session_init.required_mcp_server_count = self.required_servers.len(),
        ))
        .await;
        if failures.is_empty() {
            return Ok(());
        }

        let details = failures
            .iter()
            .map(|failure| format!("{}: {}", failure.server, failure.error))
            .collect::<Vec<_>>()
            .join("; ");
        Err(anyhow!(
            "required MCP servers failed to initialize: {details}"
        ))
    }

    pub fn new_uninitialized_with_permission_profile(
        approval_policy: &Constrained<AskForApproval>,
        permission_profile: &PermissionProfile,
        prefix_mcp_tool_names: bool,
    ) -> Self {
        Self {
            clients: HashMap::new(),
            server_definitions: HashMap::new(),
            server_metadata: HashMap::new(),
            required_servers: Vec::new(),
            tool_plugin_provenance: Arc::new(ToolPluginProvenance::default()),
            tool_catalog_revision: Arc::new(AtomicU64::new(0)),
            tool_catalog_cache: StdMutex::new(None),
            prefix_mcp_tool_names,
            elicitation_requests: ElicitationRequestManager::new(
                approval_policy.value(),
                permission_profile.clone(),
                /*reviewer*/ None,
                /*lifecycle*/ None,
                ElicitationRequestRouter::default(),
            ),
            client_reuse_context: ClientReuseContext {
                store_mode: OAuthCredentialsStoreMode::default(),
                keyring_backend_kind: AuthKeyringBackendKind::default(),
                runtime_context: McpRuntimeContext::new(
                    Arc::new(codex_exec_server::EnvironmentManager::without_environments()),
                    PathBuf::new(),
                ),
                codex_home: PathBuf::new(),
                codex_apps_tools_cache_key: CodexAppsToolsCacheKey {
                    account_id: None,
                    chatgpt_user_id: None,
                    is_workspace_account: false,
                    chatgpt_base_url: String::new(),
                    product_sku: String::new(),
                },
                client_elicitation_capability: ElicitationCapability::default(),
                supports_openai_form_elicitation: false,
            },
            shutdown_started: AtomicBool::new(false),
        }
    }

    pub fn has_servers(&self) -> bool {
        !self.clients.is_empty()
    }

    /// Monotonic identity for in-place changes to this manager's model-facing
    /// tool catalog. Replacing the manager is tracked by the owning runtime
    /// generation; this revision covers refreshes that reuse the manager.
    pub fn tool_catalog_revision(&self) -> u64 {
        self.tool_catalog_revision.load(Ordering::Acquire)
    }

    pub fn shutdown_started(&self) -> bool {
        self.shutdown_started.load(Ordering::Acquire)
    }

    pub(crate) fn contains_server(&self, server_name: &str) -> bool {
        self.clients.contains_key(server_name)
    }

    /// Stop all MCP clients with one manager-wide grace period before forcing process trees.
    pub async fn shutdown(&self) {
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let clients = self.clients.values().cloned().collect::<Vec<_>>();
        // Keep cleanup alive if an interrupt cancels the refresh that requested it.
        let shutdown_task = tokio::spawn(async move {
            shutdown_clients_with_deadline(
                clients,
                Duration::from_secs(2),
                |client| async move { client.shutdown().await },
                |client| async move { client.force_shutdown().await },
            )
            .await;
        });
        if let Err(error) = shutdown_task.await {
            warn!("MCP client shutdown task failed: {error}");
        }
    }

    pub fn server_origin(&self, server_name: &str) -> Option<&str> {
        self.server_metadata
            .get(server_name)
            .and_then(|metadata| metadata.origin.as_ref())
            .map(super::server::McpServerOrigin::as_str)
    }

    pub fn server_environment_id(&self, server_name: &str) -> Option<&str> {
        self.server_metadata
            .get(server_name)
            .map(|metadata| metadata.environment_id.as_str())
    }

    pub fn server_pollutes_memory(&self, server_name: &str) -> bool {
        self.server_metadata
            .get(server_name)
            .is_none_or(|metadata| metadata.pollutes_memory)
    }

    pub fn plugin_id_for_mcp_server_name(&self, server_name: &str) -> Option<&str> {
        self.tool_plugin_provenance
            .plugin_id_for_mcp_server_name(server_name)
    }

    pub fn is_selected_plugin_mcp_server(&self, server_name: &str) -> bool {
        self.tool_plugin_provenance
            .is_selected_plugin_mcp_server(server_name)
    }

    pub fn tool_approval_mode(
        &self,
        server_name: &str,
        tool_name: &str,
    ) -> codex_config::AppToolApproval {
        self.server_metadata
            .get(server_name)
            .map(|metadata| metadata.tool_approval_mode(tool_name))
            .unwrap_or_default()
    }

    pub fn is_host_owned_codex_apps_server(&self, server_name: &str) -> bool {
        server_name == CODEX_APPS_MCP_SERVER_NAME && self.server_metadata.contains_key(server_name)
    }

    pub fn set_approval_policy(&self, approval_policy: &Constrained<AskForApproval>) {
        if let Ok(mut policy) = self.elicitation_requests.approval_policy.lock() {
            *policy = approval_policy.value();
        }
    }

    pub fn set_permission_profile(&self, permission_profile: PermissionProfile) {
        if let Ok(mut profile) = self.elicitation_requests.permission_profile.lock() {
            *profile = permission_profile;
        }
    }

    pub fn elicitations_auto_deny(&self) -> bool {
        self.elicitation_requests.auto_deny()
    }

    pub fn set_elicitations_auto_deny(&self, auto_deny: bool) {
        self.elicitation_requests.set_auto_deny(auto_deny);
    }

    pub fn elicitation_router(&self) -> ElicitationRequestRouter {
        self.elicitation_requests.router()
    }

    pub async fn resolve_elicitation(
        &self,
        server_name: String,
        id: RequestId,
        response: ElicitationResponse,
    ) -> Result<()> {
        self.elicitation_requests
            .resolve(server_name, id, response)
            .await
    }

    pub async fn wait_for_server_ready(&self, server_name: &str, timeout: Duration) -> bool {
        let Some(async_managed_client) = self.clients.get(server_name) else {
            return false;
        };

        match tokio::time::timeout(timeout, async_managed_client.client()).await {
            Ok(Ok(_)) => true,
            Ok(Err(_)) | Err(_) => false,
        }
    }

    /// Returns an immutable aggregate tool snapshot for the current catalog revision.
    #[instrument(level = "trace", skip_all, fields(mcp_server_count = self.clients.len()))]
    pub async fn list_all_tools_snapshot(&self) -> Arc<Vec<ToolInfo>> {
        for managed_client in self.clients.values() {
            managed_client.reconnect_failed_startup().await;
        }

        loop {
            let revision = self.tool_catalog_revision();
            if let Some(tools) = self.cached_tool_catalog(revision) {
                return tools;
            }

            let tools = self.build_tool_catalog().await;
            if revision != self.tool_catalog_revision() {
                continue;
            }

            let tools = Arc::new(tools);
            *self
                .tool_catalog_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(CachedToolCatalog {
                revision,
                tools: Arc::clone(&tools),
            });
            return tools;
        }
    }

    fn cached_tool_catalog(&self, revision: u64) -> Option<Arc<Vec<ToolInfo>>> {
        self.tool_catalog_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|cached| cached.revision == revision)
            .map(|cached| Arc::clone(&cached.tools))
    }

    /// Returns all tools with model-visible names normalized.
    pub async fn list_all_tools(&self) -> Vec<ToolInfo> {
        self.list_all_tools_snapshot().await.as_ref().clone()
    }

    async fn build_tool_catalog(&self) -> Vec<ToolInfo> {
        let mut tools = Vec::new();
        let mut available_server_count = 0;
        let mut unavailable_server_count = 0;
        let mut listings = JoinSet::new();
        for (server_name, managed_client) in &self.clients {
            let server_name = server_name.clone();
            let managed_client = managed_client.clone();
            listings.spawn(async move {
                let has_cached_tools = managed_client.has_cached_tools();
                let startup_complete = managed_client
                    .startup_complete
                    .load(std::sync::atomic::Ordering::Acquire);
                let server_tools = managed_client
                    .listed_tools()
                    .instrument(trace_span!(
                        "list_tools_for_server",
                        server_name = %server_name,
                        has_cached_tools,
                        startup_complete
                    ))
                    .await;
                (
                    server_name,
                    has_cached_tools,
                    startup_complete,
                    server_tools,
                )
            });
        }
        while let Some(result) = listings.join_next().await {
            let Ok((server_name, has_cached_tools, startup_complete, server_tools)) = result else {
                unavailable_server_count += 1;
                warn!("MCP server tool listing task failed");
                continue;
            };
            let Some(server_tools) = server_tools else {
                unavailable_server_count += 1;
                trace!(
                    server_name = %server_name,
                    has_cached_tools,
                    startup_complete,
                    "MCP server tools unavailable while building tool list"
                );
                continue;
            };
            available_server_count += 1;
            tools.extend(
                server_tools
                    .into_iter()
                    .map(|tool| self.with_server_metadata(tool)),
            );
        }
        let tools = normalize_tools_for_model_with_prefix(tools, self.prefix_mcp_tool_names);
        trace!(
            available_server_count,
            unavailable_server_count,
            tool_count = tools.len(),
            "built MCP tool list"
        );
        tools
    }

    /// Returns the current information for one raw tool name without rebuilding
    /// the aggregate catalog for every configured server.
    pub async fn tool_info(&self, server_name: &str, tool_name: &str) -> Option<ToolInfo> {
        let managed_client = self.clients.get(server_name)?;
        managed_client.reconnect_failed_startup().await;
        managed_client
            .listed_tools()
            .await?
            .into_iter()
            .find(|tool| tool.tool.name == tool_name)
            .map(|tool| self.with_server_metadata(tool))
    }

    /// Force-refresh codex apps tools by bypassing the in-process cache.
    ///
    /// On success, the refreshed tools replace shared cache contents when the
    /// cache is enabled and the latest filtered tools are returned directly to
    /// the caller. On failure, existing shared cache contents remain unchanged.
    pub async fn hard_refresh_codex_apps_tools_cache(&self) -> Result<Vec<ToolInfo>> {
        let refresh_start = Instant::now();
        let managed_client = self
            .clients
            .get(CODEX_APPS_MCP_SERVER_NAME)
            .ok_or_else(|| anyhow!("unknown MCP server '{CODEX_APPS_MCP_SERVER_NAME}'"))?
            .client()
            .await
            .context("failed to get client")?;

        let list_start = Instant::now();
        let fetch_ticket = managed_client
            .codex_apps_tools_cache_context
            .as_ref()
            .map(|cache_context| cache_context.begin_fetch(CodexAppsToolsFetchSource::HardRefresh));
        let tools = list_tools_for_client_uncached(
            CODEX_APPS_MCP_SERVER_NAME,
            /*is_codex_apps_mcp_server*/ true,
            /*codex_apps_refresh_trigger*/ "explicit",
            &managed_client.client,
            managed_client.tool_timeout,
            managed_client.server_instructions.as_deref(),
        )
        .await
        .with_context(|| {
            format!("failed to refresh tools for MCP server '{CODEX_APPS_MCP_SERVER_NAME}'")
        })?;

        let tools =
            match (
                managed_client.codex_apps_tools_cache_context.as_ref(),
                fetch_ticket,
            ) {
                (Some(cache_context), Some(fetch_ticket)) => cache_context
                    .publish_if_newest_accepted(fetch_ticket, &managed_client.server_info, tools),
                (None, None) => tools,
                _ => unreachable!("Codex Apps fetch ticket requires cache context"),
            };
        emit_duration(
            MCP_TOOLS_LIST_DURATION_METRIC,
            list_start.elapsed(),
            &[("cache", "miss")],
        );
        let tools = filter_tools(tools, &managed_client.tool_filter);
        managed_client.tools.store(Arc::new(tools.clone()));
        let tools = tools.into_iter().map(|mut tool| {
            tool.tool = tool_with_model_visible_input_schema(&tool.tool);
            self.with_server_metadata(tool)
        });
        let tools = normalize_tools_for_model_with_prefix(tools, self.prefix_mcp_tool_names);
        emit_duration(
            CODEX_APPS_REFRESH_DURATION_METRIC,
            refresh_start.elapsed(),
            &[("path", "legacy"), ("trigger", "explicit")],
        );
        Ok(self.finish_tool_catalog_refresh(tools))
    }

    fn finish_tool_catalog_refresh(&self, tools: Vec<ToolInfo>) -> Vec<ToolInfo> {
        self.tool_catalog_revision.fetch_add(1, Ordering::AcqRel);
        tools
    }

    /// Returns resources and sanitized per-server failures from servers
    /// selected by `include_server`.
    pub async fn list_all_resources(
        &self,
        include_server: impl Fn(&str) -> bool,
    ) -> McpServerCollection<Vec<Resource>> {
        let mut join_set = JoinSet::new();
        let mut collection = McpServerCollection::default();
        let mut task_servers = HashMap::new();

        let clients_snapshot = &self.clients;

        for (server_name, async_managed_client) in clients_snapshot
            .iter()
            .filter(|(server_name, _)| include_server(server_name))
        {
            let server_name = server_name.clone();
            let managed_client = match async_managed_client.client().await {
                Ok(managed_client) => managed_client,
                Err(err) => {
                    let message = if err.is_authentication_required() {
                        "server requires authentication".to_string()
                    } else {
                        format!("server unavailable: {err}")
                    };
                    collection
                        .errors
                        .push(McpServerCollectionError::new(server_name, message));
                    continue;
                }
            };
            if !managed_client.server_supports_resources_capability {
                continue;
            }
            let timeout = Some(managed_client.tool_timeout);
            let client = managed_client.client.clone();

            let task_server = server_name.clone();
            let abort_handle = join_set.spawn(async move {
                let mut collected: Vec<Resource> = Vec::new();
                let mut cursor: Option<String> = None;
                let mut seen_cursors = HashSet::new();
                let mut page_count = 0usize;

                loop {
                    page_count += 1;
                    if page_count > MAX_MCP_COLLECTION_PAGES {
                        return (
                            server_name,
                            Err(anyhow!("resources/list exceeded page limit")),
                        );
                    }
                    let params = cursor.as_ref().map(|next| {
                        PaginatedRequestParams::default().with_cursor(Some(next.clone()))
                    });
                    let response = match client.list_resources(params, timeout).await {
                        Ok(result) => result,
                        Err(err) => return (server_name, Err(err)),
                    };

                    if collected.len().saturating_add(response.resources.len())
                        > MAX_MCP_COLLECTION_ITEMS
                    {
                        return (
                            server_name,
                            Err(anyhow!("resources/list exceeded item limit")),
                        );
                    }
                    collected.extend(response.resources);

                    match response.next_cursor {
                        Some(next) => {
                            if !seen_cursors.insert(next.clone()) {
                                return (
                                    server_name,
                                    Err(anyhow!("resources/list returned a repeated cursor")),
                                );
                            }
                            cursor = Some(next);
                        }
                        None => return (server_name, Ok(collected)),
                    }
                }
            });
            task_servers.insert(abort_handle.id(), task_server);
        }

        while let Some(join_res) = join_set.join_next_with_id().await {
            match join_res {
                Ok((task_id, (server_name, Ok(resources)))) => {
                    task_servers.remove(&task_id);
                    collection.results.insert(server_name, resources);
                }
                Ok((task_id, (server_name, Err(err)))) => {
                    task_servers.remove(&task_id);
                    warn!("Failed to list resources for MCP server '{server_name}': {err:#}");
                    collection.errors.push(McpServerCollectionError::new(
                        server_name,
                        format!("resources/list failed: {err}"),
                    ));
                }
                Err(err) => {
                    warn!("Task panic when listing resources for MCP server: {err:#}");
                    let server_name = task_servers
                        .remove(&err.id())
                        .unwrap_or_else(|| "unknown".to_string());
                    collection.errors.push(McpServerCollectionError::new(
                        server_name,
                        "resource listing task failed",
                    ));
                }
            }
        }

        collection
    }

    /// Returns the first resource page from servers selected by
    /// `include_server`. Callers can continue an unfinished server catalog with
    /// [`Self::list_resources`] and that server's returned cursor.
    pub async fn list_resource_pages(
        &self,
        include_server: impl Fn(&str) -> bool,
    ) -> McpServerCollection<ListResourcesResult> {
        let mut join_set = JoinSet::new();
        let mut collection = McpServerCollection::default();
        let mut task_servers = HashMap::new();

        for (server_name, async_managed_client) in self
            .clients
            .iter()
            .filter(|(server_name, _)| include_server(server_name))
        {
            let server_name = server_name.clone();
            let managed_client = match async_managed_client.client().await {
                Ok(managed_client) => managed_client,
                Err(err) => {
                    let message = if err.is_authentication_required() {
                        "server requires authentication".to_string()
                    } else {
                        format!("server unavailable: {err}")
                    };
                    collection
                        .errors
                        .push(McpServerCollectionError::new(server_name, message));
                    continue;
                }
            };
            if !managed_client.server_supports_resources_capability {
                continue;
            }
            let client = managed_client.client.clone();
            let timeout = Some(managed_client.tool_timeout);

            let task_server = server_name.clone();
            let abort_handle = join_set.spawn(async move {
                (
                    server_name,
                    client.list_resources(/*params*/ None, timeout).await,
                )
            });
            task_servers.insert(abort_handle.id(), task_server);
        }

        while let Some(join_res) = join_set.join_next_with_id().await {
            match join_res {
                Ok((task_id, (server_name, Ok(page)))) => {
                    task_servers.remove(&task_id);
                    collection.results.insert(server_name, page);
                }
                Ok((task_id, (server_name, Err(err)))) => {
                    task_servers.remove(&task_id);
                    warn!("Failed to list resources for MCP server '{server_name}': {err:#}");
                    collection.errors.push(McpServerCollectionError::new(
                        server_name,
                        format!("resources/list failed: {err}"),
                    ));
                }
                Err(err) => {
                    warn!("Task panic when listing resources for MCP server: {err:#}");
                    let server_name = task_servers
                        .remove(&err.id())
                        .unwrap_or_else(|| "unknown".to_string());
                    collection.errors.push(McpServerCollectionError::new(
                        server_name,
                        "resource listing task failed",
                    ));
                }
            }
        }

        collection
    }

    /// Returns resource templates and sanitized per-server failures from
    /// servers selected by `include_server`.
    pub async fn list_all_resource_templates(
        &self,
        include_server: impl Fn(&str) -> bool,
    ) -> McpServerCollection<Vec<ResourceTemplate>> {
        let mut join_set = JoinSet::new();
        let mut collection = McpServerCollection::default();
        let mut task_servers = HashMap::new();

        let clients_snapshot = &self.clients;

        for (server_name, async_managed_client) in clients_snapshot
            .iter()
            .filter(|(server_name, _)| include_server(server_name))
        {
            let server_name = server_name.clone();
            let managed_client = match async_managed_client.client().await {
                Ok(managed_client) => managed_client,
                Err(err) => {
                    let message = if err.is_authentication_required() {
                        "server requires authentication".to_string()
                    } else {
                        format!("server unavailable: {err}")
                    };
                    collection
                        .errors
                        .push(McpServerCollectionError::new(server_name, message));
                    continue;
                }
            };
            if !managed_client.server_supports_resources_capability {
                continue;
            }
            let client = managed_client.client.clone();
            let timeout = Some(managed_client.tool_timeout);

            let task_server = server_name.clone();
            let abort_handle = join_set.spawn(async move {
                let mut collected: Vec<ResourceTemplate> = Vec::new();
                let mut cursor: Option<String> = None;
                let mut seen_cursors = HashSet::new();
                let mut page_count = 0usize;

                loop {
                    page_count += 1;
                    if page_count > MAX_MCP_COLLECTION_PAGES {
                        return (
                            server_name,
                            Err(anyhow!("resources/templates/list exceeded page limit")),
                        );
                    }
                    let params = cursor.as_ref().map(|next| {
                        PaginatedRequestParams::default().with_cursor(Some(next.clone()))
                    });
                    let response = match client.list_resource_templates(params, timeout).await {
                        Ok(result) => result,
                        Err(err) => return (server_name, Err(err)),
                    };

                    if collected
                        .len()
                        .saturating_add(response.resource_templates.len())
                        > MAX_MCP_COLLECTION_ITEMS
                    {
                        return (
                            server_name,
                            Err(anyhow!("resources/templates/list exceeded item limit")),
                        );
                    }
                    collected.extend(response.resource_templates);

                    match response.next_cursor {
                        Some(next) => {
                            if !seen_cursors.insert(next.clone()) {
                                return (
                                    server_name,
                                    Err(anyhow!(
                                        "resources/templates/list returned a repeated cursor"
                                    )),
                                );
                            }
                            cursor = Some(next);
                        }
                        None => return (server_name, Ok(collected)),
                    }
                }
            });
            task_servers.insert(abort_handle.id(), task_server);
        }

        while let Some(join_res) = join_set.join_next_with_id().await {
            match join_res {
                Ok((task_id, (server_name, Ok(templates)))) => {
                    task_servers.remove(&task_id);
                    collection.results.insert(server_name, templates);
                }
                Ok((task_id, (server_name, Err(err)))) => {
                    task_servers.remove(&task_id);
                    warn!(
                        "Failed to list resource templates for MCP server '{server_name}': {err:#}"
                    );
                    collection.errors.push(McpServerCollectionError::new(
                        server_name,
                        format!("resources/templates/list failed: {err}"),
                    ));
                }
                Err(err) => {
                    warn!("Task panic when listing resource templates for MCP server: {err:#}");
                    let server_name = task_servers
                        .remove(&err.id())
                        .unwrap_or_else(|| "unknown".to_string());
                    collection.errors.push(McpServerCollectionError::new(
                        server_name,
                        "resource-template listing task failed",
                    ));
                }
            }
        }

        collection
    }

    /// Returns the first resource-template page from servers selected by
    /// `include_server`. Callers can continue an unfinished server catalog with
    /// [`Self::list_resource_templates`] and that server's returned cursor.
    pub async fn list_resource_template_pages(
        &self,
        include_server: impl Fn(&str) -> bool,
    ) -> McpServerCollection<ListResourceTemplatesResult> {
        let mut join_set = JoinSet::new();
        let mut collection = McpServerCollection::default();
        let mut task_servers = HashMap::new();

        for (server_name, async_managed_client) in self
            .clients
            .iter()
            .filter(|(server_name, _)| include_server(server_name))
        {
            let server_name = server_name.clone();
            let managed_client = match async_managed_client.client().await {
                Ok(managed_client) => managed_client,
                Err(err) => {
                    let message = if err.is_authentication_required() {
                        "server requires authentication".to_string()
                    } else {
                        format!("server unavailable: {err}")
                    };
                    collection
                        .errors
                        .push(McpServerCollectionError::new(server_name, message));
                    continue;
                }
            };
            if !managed_client.server_supports_resources_capability {
                continue;
            }
            let client = managed_client.client.clone();
            let timeout = Some(managed_client.tool_timeout);

            let task_server = server_name.clone();
            let abort_handle = join_set.spawn(async move {
                (
                    server_name,
                    client.list_resource_templates(None, timeout).await,
                )
            });
            task_servers.insert(abort_handle.id(), task_server);
        }

        while let Some(join_res) = join_set.join_next_with_id().await {
            match join_res {
                Ok((task_id, (server_name, Ok(page)))) => {
                    task_servers.remove(&task_id);
                    collection.results.insert(server_name, page);
                }
                Ok((task_id, (server_name, Err(err)))) => {
                    task_servers.remove(&task_id);
                    warn!(
                        "Failed to list resource templates for MCP server '{server_name}': {err:#}"
                    );
                    collection.errors.push(McpServerCollectionError::new(
                        server_name,
                        format!("resources/templates/list failed: {err}"),
                    ));
                }
                Err(err) => {
                    warn!("Task panic when listing resource templates for MCP server: {err:#}");
                    let server_name = task_servers
                        .remove(&err.id())
                        .unwrap_or_else(|| "unknown".to_string());
                    collection.errors.push(McpServerCollectionError::new(
                        server_name,
                        "resource-template listing task failed",
                    ));
                }
            }
        }

        collection
    }

    /// Invoke the tool indicated by the (server, tool) pair.
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<serde_json::Value>,
        meta: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let client = self.client_by_name(server).await?;
        if !client.tool_filter.allows(tool) {
            return Err(anyhow!(
                "tool '{tool}' is disabled for MCP server '{server}'"
            ));
        }

        let result: rmcp::model::CallToolResult = client
            .client
            .call_tool(tool.to_string(), arguments, meta, Some(client.tool_timeout))
            .await
            .with_context(|| format!("tool call failed for `{server}/{tool}`"))?;

        let content = result
            .content
            .into_iter()
            .map(|content| {
                serde_json::to_value(content)
                    .unwrap_or_else(|_| serde_json::Value::String("<content>".to_string()))
            })
            .collect();

        Ok(CallToolResult {
            content,
            structured_content: result.structured_content,
            is_error: result.is_error,
            meta: result.meta.and_then(|meta| serde_json::to_value(meta).ok()),
        })
    }

    pub async fn server_supports_sandbox_state_meta_capability(
        &self,
        server: &str,
    ) -> Result<bool> {
        Ok(self
            .client_by_name(server)
            .await?
            .server_supports_sandbox_state_meta_capability)
    }

    /// List resources from the specified server.
    pub async fn list_resources(
        &self,
        server: &str,
        params: Option<PaginatedRequestParams>,
    ) -> Result<ListResourcesResult> {
        let managed = self.client_by_name(server).await?;
        let timeout = Some(managed.tool_timeout);

        managed
            .client
            .list_resources(params, timeout)
            .await
            .with_context(|| format!("resources/list failed for `{server}`"))
    }

    /// List resource templates from the specified server.
    pub async fn list_resource_templates(
        &self,
        server: &str,
        params: Option<PaginatedRequestParams>,
    ) -> Result<ListResourceTemplatesResult> {
        let managed = self.client_by_name(server).await?;
        let client = managed.client.clone();
        let timeout = Some(managed.tool_timeout);

        client
            .list_resource_templates(params, timeout)
            .await
            .with_context(|| format!("resources/templates/list failed for `{server}`"))
    }

    /// Read a resource from the specified server.
    pub async fn read_resource(
        &self,
        server: &str,
        params: ReadResourceRequestParams,
    ) -> Result<ReadResourceResult> {
        let managed = self.client_by_name(server).await?;
        let client = managed.client.clone();
        let timeout = Some(managed.tool_timeout);
        let uri = params.uri.clone();

        client
            .read_resource(params, timeout)
            .await
            .with_context(|| format!("resources/read failed for `{server}` ({uri})"))
    }

    /// Returns presentation metadata without waiting for uncached clients still initializing.
    /// Cached values will be used if available and the server is still starting up.
    pub(crate) async fn list_available_server_infos(&self) -> HashMap<String, McpServerInfo> {
        let mut server_infos = HashMap::new();
        for (server_name, client) in &self.clients {
            if !client.startup_complete.load(Ordering::Acquire) {
                if let Some(server_info) = client.cached_server_info.clone() {
                    server_infos.insert(server_name.clone(), server_info);
                }
                continue;
            }
            match client.client().await {
                Ok(managed_client) => {
                    server_infos.insert(server_name.clone(), managed_client.server_info);
                }
                Err(_) => {
                    if let Some(server_info) = client.cached_server_info.clone() {
                        server_infos.insert(server_name.clone(), server_info);
                    }
                }
            }
        }
        server_infos
    }

    fn with_server_metadata(&self, mut tool: ToolInfo) -> ToolInfo {
        let Some(metadata) = self.server_metadata.get(&tool.server_name) else {
            tool.supports_parallel_tool_calls = false;
            tool.server_origin = None;
            return tool;
        };

        tool.supports_parallel_tool_calls = metadata.supports_parallel_tool_calls;
        tool.server_origin = metadata
            .origin
            .as_ref()
            .map(|origin| origin.as_str().to_string());
        tool
    }

    async fn client_by_name(&self, name: &str) -> Result<ManagedClient> {
        self.clients
            .get(name)
            .ok_or_else(|| anyhow!("unknown MCP server '{name}'"))?
            .client()
            .await
            .context("failed to get client")
    }

    #[cfg(test)]
    fn new_uninitialized(
        approval_policy: &Constrained<AskForApproval>,
        permission_profile: &Constrained<PermissionProfile>,
        prefix_mcp_tool_names: bool,
    ) -> Self {
        Self::new_uninitialized_with_permission_profile(
            approval_policy,
            permission_profile.get(),
            prefix_mcp_tool_names,
        )
    }
}

impl Drop for McpConnectionManager {
    fn drop(&mut self) {
        if !self.shutdown_started.load(Ordering::Acquire) {
            for client in self.clients.values() {
                client.release_manager_without_shutdown();
            }
        }
        self.clients.clear();
    }
}

/// Makes ChatGPT authentication available to servers that explicitly opt in.
/// The HTTP transport applies it only when no configured authorization resolves.
fn chatgpt_auth_provider_for_server(
    server: &EffectiveMcpServer,
    chatgpt_auth_provider: Option<SharedAuthProvider>,
) -> Option<SharedAuthProvider> {
    if !server
        .configured_config()
        .is_some_and(|config| matches!(&config.auth, McpServerAuth::ChatGpt))
    {
        return None;
    }
    chatgpt_auth_provider
}

fn should_share_codex_apps_tools_cache(server_name: &str, uses_env_bearer_token: bool) -> bool {
    server_name == CODEX_APPS_MCP_SERVER_NAME && !uses_env_bearer_token
}

async fn emit_update(
    submit_id: &str,
    tx_event: &Sender<Event>,
    update: McpStartupUpdateEvent,
) -> Result<(), async_channel::SendError<Event>> {
    tx_event
        .send(Event {
            id: submit_id.to_string(),
            msg: EventMsg::McpStartupUpdate(update),
        })
        .await
}

fn mcp_startup_failure_reason(
    entry: Option<&McpAuthStatusEntry>,
    error: &StartupOutcomeError,
) -> Option<McpStartupFailureReason> {
    if !error.is_authentication_required() {
        return None;
    }

    match entry.map(|entry| entry.auth_state) {
        Some(McpAuthState::LoggedOut(McpLoginRequirement::Reauthentication)) => {
            Some(McpStartupFailureReason::ReauthenticationRequired)
        }
        Some(
            McpAuthState::Unsupported
            | McpAuthState::LoggedOut(McpLoginRequirement::Login)
            | McpAuthState::BearerToken
            | McpAuthState::OAuth,
        )
        | None => None,
    }
}

fn mcp_init_error_display(
    server_name: &str,
    entry: Option<&McpAuthStatusEntry>,
    err: &StartupOutcomeError,
) -> String {
    if let Some(McpServerTransportConfig::StreamableHttp {
        url,
        bearer_token_env_var,
        http_headers,
        ..
    }) = entry.and_then(|entry| entry.config.as_ref().map(|config| &config.transport))
        && url == "https://api.githubcopilot.com/mcp/"
        && bearer_token_env_var.is_none()
        && http_headers.as_ref().map(HashMap::is_empty).unwrap_or(true)
    {
        format!(
            "GitHub MCP does not support OAuth. Log in by adding a personal access token (https://github.com/settings/personal-access-tokens) to your environment and config.toml:\n[mcp_servers.{server_name}]\nbearer_token_env_var = CODEX_GITHUB_PERSONAL_ACCESS_TOKEN"
        )
    } else if is_mcp_client_auth_required_error(err) {
        format!(
            "The {server_name} MCP server is not logged in. Run `codex mcp login {server_name}`."
        )
    } else if is_mcp_client_startup_timeout_error(err) {
        let startup_timeout_secs = match entry {
            Some(entry) => match entry
                .config
                .as_ref()
                .and_then(|config| config.startup_timeout_sec)
            {
                Some(timeout) => timeout,
                None => DEFAULT_STARTUP_TIMEOUT,
            },
            None => DEFAULT_STARTUP_TIMEOUT,
        }
        .as_secs();
        format!(
            "MCP client for `{server_name}` timed out after {startup_timeout_secs} seconds. Add or adjust `startup_timeout_sec` in your config.toml:\n[mcp_servers.{server_name}]\nstartup_timeout_sec = XX"
        )
    } else {
        format!("MCP client for `{server_name}` failed to start: {err:#}")
    }
}

fn startup_outcome_error_message(error: StartupOutcomeError) -> String {
    match error {
        StartupOutcomeError::Cancelled => "MCP startup cancelled".to_string(),
        StartupOutcomeError::Failed { error, .. } => error,
    }
}

fn is_mcp_client_auth_required_error(error: &StartupOutcomeError) -> bool {
    error.is_authentication_required()
}

fn is_mcp_client_startup_timeout_error(error: &StartupOutcomeError) -> bool {
    error.is_timeout()
}

#[cfg(test)]
#[path = "connection_manager_tests.rs"]
mod tests;
