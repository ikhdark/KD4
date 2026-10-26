//! Shared app-server client facade for CLI surfaces.
//!
//! This crate presents in-process and remote app-server transports behind a
//! single async API used by surfaces like TUI and exec. It centralizes:
//!
//! - Runtime startup and initialize-capabilities handshake.
//! - Typed caller-provided startup identity (`SessionSource` + client name).
//! - Typed and raw request/notification dispatch.
//! - Server request resolution and rejection.
//! - Event consumption with backpressure signaling ([`AppServerEvent::Lagged`]).
//! - Bounded graceful shutdown with abort fallback.
//!
//! Each transport interposes a worker task between the caller and the underlying
//! runtime or connection. Command/event queues and dispatched requests awaiting
//! responses are each bounded by the configured channel capacity. Excess requests
//! receive a retryable `inFlightTaskCapacity` overload error before dispatch.
//! Cancelling a dispatched request's waiter does not cancel server-side work or
//! release its request ID/capacity; the response or transport shutdown does that.

mod path;
mod remote;
mod thread_lifecycle;

use std::error::Error;
use std::fmt;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::sync::Arc;
use std::time::Duration;

pub use codex_app_server::app_server_control_socket_path;
pub use codex_app_server::in_process::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
pub use codex_app_server::in_process::InProcessServerEvent;
use codex_app_server::in_process::InProcessStartArgs;
use codex_app_server::in_process::LogDbLayer;
pub use codex_app_server::in_process::StateDbHandle;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result as JsonRpcResult;
use codex_app_server_protocol::ServerBuildInfo;
use codex_app_server_protocol::ServerCapabilities;
use codex_app_server_protocol::ServerLocalWatermark;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ServerRuntimeWarning;
use codex_app_server_protocol::server_notification_requires_delivery;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::thread_config_loader_for_endpoint;
use codex_core::config::Config;
pub use codex_core::otel_init::build_provider as build_otel_provider;
pub use codex_exec_server::EnvironmentManager;
pub use codex_exec_server::ExecServerRuntimePaths;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::SessionSource;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use toml::Value as TomlValue;
use tracing::warn;

pub use crate::path::AppServerPath;
pub use crate::remote::RemoteAppServerClient;
pub use crate::remote::RemoteAppServerConnectArgs;
pub use crate::remote::RemoteAppServerEndpoint;
pub use crate::thread_lifecycle::ThreadLifecycleOverrides;
pub use crate::thread_lifecycle::thread_config_overrides_from_config;
pub use crate::thread_lifecycle::thread_fork_params_from_config;
pub use crate::thread_lifecycle::thread_resume_params_from_config;
pub use crate::thread_lifecycle::thread_start_params_from_config;

/// Transitional access to core-only embedded app-server types.
///
/// New TUI behavior should prefer the app-server protocol methods. This
/// module exists so clients can remove a direct `codex-core` dependency
/// while legacy startup/config paths are migrated to RPCs.
pub mod legacy_core {
    pub mod config {
        // Keep the remaining client dependencies explicit so new core APIs do
        // not automatically become part of this migration bridge.
        pub use codex_core::config::Config;
        pub use codex_core::config::ConfigBuilder;
        pub use codex_core::config::ConfigOverrides;
        pub use codex_core::config::ConfigTomlLoadResult;
        pub use codex_core::config::PermissionProfileCatalogEntry;
        pub use codex_core::config::PermissionProfileSnapshot;
        pub use codex_core::config::Permissions;
        pub use codex_core::config::TerminalResizeReflowConfig;
        pub use codex_core::config::TerminalResizeReflowMaxRows;
        pub use codex_core::config::find_codex_home_async;
        pub use codex_core::config::load_config_toml_with_layer_stack;
        pub use codex_core::config::resolve_bootstrap_auth_keyring_backend_kind;
        pub use codex_core::config::resolve_bootstrap_auth_route_config;
        pub use codex_core::config::resolve_oss_provider;
        pub use codex_core::config::resolve_profile_v2_config_path;

        pub mod edit {
            pub use codex_core::config::edit::ConfigEdit;
            pub use codex_core::config::edit::ConfigEditsBuilder;
            pub use codex_core::config::edit::keymap_binding_clear_edit;
            pub use codex_core::config::edit::keymap_bindings_edit;
            pub use codex_core::config::edit::status_line_items_edit;
            pub use codex_core::config::edit::status_line_use_colors_edit;
            pub use codex_core::config::edit::syntax_theme_edit;
            pub use codex_core::config::edit::terminal_title_items_edit;
            pub use codex_core::config::edit::tui_pet_edit;
        }
    }
}

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);


/// Raw app-server request result for typed in-process requests.
///
/// Even on the in-process path, successful responses still travel back through
/// the same JSON-RPC result envelope used by socket/stdio transports because
/// `MessageProcessor` continues to produce that shape internally.
pub type RequestResult = std::result::Result<JsonRpcResult, JSONRPCErrorError>;

/// Initialize metadata retained uniformly by embedded and remote transports.
///
/// The raw result remains available because remote paths may use a different
/// platform's absolute-path syntax, which cannot be safely deserialized into
/// this process's platform-specific path type.
#[derive(Debug, Clone)]
pub struct AppServerInitializeResponse {
    raw: JsonRpcResult,
    user_agent: Option<String>,
    codex_home: Option<String>,
    build_info: Option<ServerBuildInfo>,
    runtime_warnings: Vec<ServerRuntimeWarning>,
    server_capabilities: Option<ServerCapabilities>,
    local_watermark: Option<ServerLocalWatermark>,
}

impl AppServerInitializeResponse {
    pub(crate) fn from_json(raw: JsonRpcResult) -> Self {
        let user_agent = raw
            .get("userAgent")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let codex_home = raw
            .get("codexHome")
            .and_then(serde_json::Value::as_str)
            .filter(|codex_home| !codex_home.is_empty())
            .map(str::to_string);
        let build_info = raw
            .get("buildInfo")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok());
        let runtime_warnings = raw
            .pointer("/runtimeInfo/warnings")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        let server_capabilities = raw
            .get("serverCapabilities")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok());
        let local_watermark = raw
            .get("localWatermark")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok());

        Self {
            raw,
            user_agent,
            codex_home,
            build_info,
            runtime_warnings,
            server_capabilities,
            local_watermark,
        }
    }

    pub fn raw(&self) -> &JsonRpcResult {
        &self.raw
    }

    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    pub fn server_version(&self) -> Option<&str> {
        let (_, rest) = self.user_agent()?.split_once('/')?;
        rest.split_whitespace().next()
    }

    pub fn codex_home(&self) -> Option<&str> {
        self.codex_home.as_deref()
    }

    pub fn build_info(&self) -> Option<&ServerBuildInfo> {
        self.build_info.as_ref()
    }

    pub fn runtime_info(&self) -> Option<&serde_json::Value> {
        self.raw.get("runtimeInfo")
    }

    pub fn runtime_warnings(&self) -> &[ServerRuntimeWarning] {
        &self.runtime_warnings
    }

    pub fn server_capabilities(&self) -> Option<&ServerCapabilities> {
        self.server_capabilities.as_ref()
    }

    pub fn local_watermark(&self) -> Option<&ServerLocalWatermark> {
        self.local_watermark.as_ref()
    }
}

pub fn format_runtime_warning(warning: &ServerRuntimeWarning) -> String {
    match warning.action.as_deref() {
        Some(action) => format!(
            "App-server warning [{}]: {}\nSuggested action: {action}",
            warning.code, warning.message
        ),
        None => format!("App-server warning [{}]: {}", warning.code, warning.message),
    }
}

#[derive(Debug, Clone)]
// Keep the client event API aligned across remote and in-process transports.
#[allow(clippy::large_enum_variant)]
pub enum AppServerEvent {
    Lagged { skipped: usize },
    ServerNotification(ServerNotification),
    ServerRequest(ServerRequest),
    Disconnected { message: String },
}

impl From<InProcessServerEvent> for AppServerEvent {
    fn from(value: InProcessServerEvent) -> Self {
        match value {
            InProcessServerEvent::Lagged { skipped } => Self::Lagged { skipped },
            InProcessServerEvent::ServerNotification(notification) => {
                Self::ServerNotification(notification)
            }
            InProcessServerEvent::ServerRequest(request) => Self::ServerRequest(request),
        }
    }
}

fn event_requires_delivery(event: &InProcessServerEvent) -> bool {
    // These transcript and terminal events must remain lossless. Dropping
    // streamed assistant text or the authoritative completed item can leave
    // the TUI with permanently corrupted markdown, while dropping completion
    // notifications can leave surfaces waiting forever.
    match event {
        InProcessServerEvent::ServerNotification(notification) => {
            server_notification_requires_delivery(notification)
        }
        _ => false,
    }
}

fn skipped_event_count(event: &InProcessServerEvent) -> usize {
    match event {
        InProcessServerEvent::Lagged { skipped } => *skipped,
        _ => 1,
    }
}

/// Outcome of attempting to forward a single event to the consumer channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardEventResult {
    /// The event was delivered, retained, or intentionally dropped; the stream is healthy.
    Continue,
    /// The consumer channel is closed; the caller should stop producing events.
    DisableStream,
}

/// Forwards a single in-process event to the consumer, respecting the
/// lossless/best-effort split.
///
/// Lossless events use available channel capacity immediately. Otherwise they
/// and preceding lag markers are retained until the consumer drains capacity,
/// with upstream reads paused and delivery selected alongside commands.
/// Best-effort events use `try_send` and increment `skipped_events` on failure.
///
/// If a dropped event is a `ServerRequest`, `reject_server_request` is called
/// so the server does not wait for a response that will never come.
fn forward_in_process_event<F>(
    event_tx: &mpsc::Sender<InProcessServerEvent>,
    pending_delivery: &mut std::collections::VecDeque<InProcessServerEvent>,
    skipped_events: &mut usize,
    event: InProcessServerEvent,
    mut reject_server_request: F,
) -> ForwardEventResult
where
    F: FnMut(ServerRequest),
{
    if event_tx.is_closed() {
        if let InProcessServerEvent::ServerRequest(request) = event {
            reject_server_request(request);
        }
        return ForwardEventResult::DisableStream;
    }
    if event_requires_delivery(&event) {
        // Avoid another worker select pass when no retained event or lag marker
        // needs to precede this one. A full channel still keeps delivery lossless.
        let event = if pending_delivery.is_empty() && *skipped_events == 0 {
            match event_tx.try_send(event) {
                Ok(()) => return ForwardEventResult::Continue,
                Err(mpsc::error::TrySendError::Full(event)) => event,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return ForwardEventResult::DisableStream;
                }
            }
        } else {
            event
        };
        if *skipped_events > 0 {
            pending_delivery.push_back(InProcessServerEvent::Lagged {
                skipped: *skipped_events,
            });
            *skipped_events = 0;
        }
        pending_delivery.push_back(event);
        return ForwardEventResult::Continue;
    }
    let mut sent_lagged = false;
    if *skipped_events > 0 {
        match event_tx.try_send(InProcessServerEvent::Lagged {
            skipped: *skipped_events,
        }) {
            Ok(()) => {
                *skipped_events = 0;
                sent_lagged = true;
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                *skipped_events = skipped_events.saturating_add(skipped_event_count(&event));
                if let InProcessServerEvent::ServerRequest(request) = event {
                    reject_server_request(request);
                }
                return ForwardEventResult::Continue;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                if let InProcessServerEvent::ServerRequest(request) = event {
                    reject_server_request(request);
                }
                return ForwardEventResult::DisableStream;
            }
        }
    }

    match event_tx.try_send(event) {
        Ok(()) => ForwardEventResult::Continue,
        Err(mpsc::error::TrySendError::Full(event)) => {
            if sent_lagged && matches!(&event, InProcessServerEvent::ServerRequest(_)) {
                // The advisory marker consumed capacity this request could have
                // used. Retain just its adjacent request, in order, rather than
                // manufacturing overload. The worker pauses reads while retained.
                pending_delivery.push_back(event);
                return ForwardEventResult::Continue;
            }
            *skipped_events = skipped_events.saturating_add(skipped_event_count(&event));
            warn!("dropping in-process app-server event because consumer queue is full");
            if let InProcessServerEvent::ServerRequest(request) = event {
                reject_server_request(request);
            }
            ForwardEventResult::Continue
        }
        Err(mpsc::error::TrySendError::Closed(event)) => {
            if let InProcessServerEvent::ServerRequest(request) = event {
                reject_server_request(request);
            }
            ForwardEventResult::DisableStream
        }
    }
}

/// Layered error for [`InProcessAppServerClient::request_typed`].
///
/// This keeps transport failures, server-side JSON-RPC failures, and response
/// decode failures distinct so callers can decide whether to retry, surface a
/// server error, or treat the response as an internal request/response mismatch.
#[derive(Debug)]
pub enum TypedRequestError {
    Transport {
        method: String,
        source: IoError,
    },
    Server {
        method: String,
        source: JSONRPCErrorError,
    },
    Deserialize {
        method: String,
        source: serde_json::Error,
    },
}

impl fmt::Display for TypedRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport { method, source } => {
                write!(f, "{method} transport error: {source}")
            }
            Self::Server { method, source } => {
                write!(
                    f,
                    "{method} failed: {} (code {})",
                    source.message, source.code
                )?;
                if let Some(data) = source.data.as_ref() {
                    write!(f, ", data: {data}")?;
                }
                Ok(())
            }
            Self::Deserialize { method, source } => {
                write!(f, "{method} response decode error: {source}")
            }
        }
    }
}

impl Error for TypedRequestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport { source, .. } => Some(source),
            Self::Server { .. } => None,
            Self::Deserialize { source, .. } => Some(source),
        }
    }
}

impl TypedRequestError {
    /// Returns the structured JSON-RPC error, including local pre-dispatch overloads.
    pub fn server_error(&self) -> Option<&JSONRPCErrorError> {
        match self {
            Self::Server { source, .. } => Some(source),
            Self::Transport { .. } | Self::Deserialize { .. } => None,
        }
    }
}

#[derive(Clone)]
pub struct InProcessClientStartArgs {
    /// Resolved argv0 dispatch paths used by command execution internals.
    pub arg0_paths: Arg0DispatchPaths,
    /// Shared config used to initialize app-server runtime.
    pub config: Arc<Config>,
    /// CLI config overrides that are already parsed into TOML values.
    pub cli_overrides: Vec<(String, TomlValue)>,
    /// Loader override knobs used by config API paths.
    pub loader_overrides: LoaderOverrides,
    /// Whether config API paths should reject unknown config fields.
    pub strict_config: bool,
    /// Preloaded cloud config bundle provider.
    pub cloud_config_bundle: CloudConfigBundleLoader,
    /// Feedback sink used by app-server/core telemetry and logs.
    pub feedback: CodexFeedback,
    /// SQLite tracing layer used to flush recently emitted logs before feedback upload.
    pub log_db: Option<LogDbLayer>,
    /// Process-wide SQLite state handle shared with the embedded app-server.
    pub state_db: Option<StateDbHandle>,
    /// Environment manager used by core execution and filesystem operations.
    pub environment_manager: Arc<EnvironmentManager>,
    /// Additional startup warnings emitted after initialize succeeds.
    ///
    /// Warnings already present on [`Config::startup_warnings`] are added by
    /// the client so embedders do not need to register them independently.
    pub config_warnings: Vec<ConfigWarningNotification>,
    /// Session source recorded in app-server thread metadata.
    pub session_source: SessionSource,
    /// Whether auth loading should honor the `CODEX_API_KEY` environment variable.
    pub enable_codex_api_key_env: bool,
    /// Client name reported during initialize.
    pub client_name: String,
    /// Client version reported during initialize.
    pub client_version: String,
    /// Whether experimental APIs are requested at initialize time.
    pub experimental_api: bool,
    /// Whether MCP servers may send `openai/form` elicitation requests.
    pub mcp_server_openai_form_elicitation: bool,
    /// Notification methods this client opts out of receiving.
    pub opt_out_notification_methods: Vec<String>,
    /// Capacity for command/event channels and dispatched requests awaiting replies
    /// (clamped to at least 1). Excess requests receive a retryable overload error.
    pub channel_capacity: usize,
}

fn runtime_config_warnings(
    config: &Config,
    additional_warnings: Vec<ConfigWarningNotification>,
) -> Vec<ConfigWarningNotification> {
    let mut warnings = config
        .startup_warnings
        .iter()
        .map(|warning| ConfigWarningNotification {
            summary: warning.clone(),
            details: None,
            path: None,
            range: None,
        })
        .collect::<Vec<_>>();
    warnings.extend(additional_warnings);
    warnings
}

fn initialize_params(
    client_name: &str,
    client_version: &str,
    experimental_api: bool,
    mcp_server_openai_form_elicitation: bool,
    opt_out_notification_methods: &[String],
) -> InitializeParams {
    InitializeParams {
        client_info: ClientInfo {
            name: client_name.to_string(),
            title: None,
            version: client_version.to_string(),
        },
        capabilities: Some(InitializeCapabilities {
            experimental_api,
            request_attestation: false,
            opt_out_notification_methods: (!opt_out_notification_methods.is_empty())
                .then(|| opt_out_notification_methods.to_vec()),
            mcp_server_openai_form_elicitation,
        }),
    }
}

impl InProcessClientStartArgs {
    /// Builds initialize params from caller-provided metadata.
    pub fn initialize_params(&self) -> InitializeParams {
        initialize_params(
            &self.client_name,
            &self.client_version,
            self.experimental_api,
            self.mcp_server_openai_form_elicitation,
            &self.opt_out_notification_methods,
        )
    }

    fn into_runtime_start_args(self) -> InProcessStartArgs {
        let initialize = self.initialize_params();
        let thread_config_loader = thread_config_loader_for_endpoint(
            self.config.experimental_thread_config_endpoint.as_deref(),
        );
        let config_warnings = runtime_config_warnings(&self.config, self.config_warnings);
        InProcessStartArgs {
            arg0_paths: self.arg0_paths,
            config: self.config,
            cli_overrides: self.cli_overrides,
            loader_overrides: self.loader_overrides,
            strict_config: self.strict_config,
            cloud_config_bundle: self.cloud_config_bundle,
            thread_config_loader,
            feedback: self.feedback,
            log_db: self.log_db,
            state_db: self.state_db,
            environment_manager: self.environment_manager,
            config_warnings,
            session_source: self.session_source,
            enable_codex_api_key_env: self.enable_codex_api_key_env,
            initialize,
            channel_capacity: self.channel_capacity,
        }
    }
}

/// Internal command sent from public facade methods to the worker task.
///
/// Each variant carries a oneshot sender so the caller can `await` the
/// result without holding a mutable reference to the client.
enum ClientCommand {
    Request {
        request: Box<ClientRequest>,
        trace: Option<codex_protocol::protocol::W3cTraceContext>,
        response_tx: oneshot::Sender<IoResult<RequestResult>>,
    },
    Notify {
        notification: ClientNotification,
        response_tx: oneshot::Sender<IoResult<()>>,
    },
    ResolveServerRequest {
        request_id: RequestId,
        result: JsonRpcResult,
        response_tx: oneshot::Sender<IoResult<()>>,
    },
    RejectServerRequest {
        request_id: RequestId,
        error: JSONRPCErrorError,
        response_tx: oneshot::Sender<IoResult<()>>,
    },
    Shutdown {
        response_tx: oneshot::Sender<IoResult<()>>,
    },
}

impl ClientCommand {
    /// A request whose caller was cancelled before the worker dequeued it must
    /// not run later and hold up newer work with a result nobody can receive.
    fn is_abandoned_request(&self) -> bool {
        matches!(
            self,
            Self::Request { response_tx, .. } if response_tx.is_closed()
        )
    }
}

/// Async facade over the in-process app-server runtime.
///
/// This type owns a worker task that bridges between:
/// - caller-facing async `mpsc` channels used by TUI/exec
/// - [`codex_app_server::in_process::InProcessClientHandle`], which speaks to
///   the embedded `MessageProcessor`
///
/// The facade intentionally preserves the server's request/notification/event
/// model instead of exposing direct core runtime handles. That keeps in-process
/// callers aligned with app-server behavior while still avoiding a process
/// boundary.
pub struct InProcessAppServerClient {
    command_tx: mpsc::Sender<ClientCommand>,
    event_rx: mpsc::Receiver<InProcessServerEvent>,
    initialize_response: AppServerInitializeResponse,
    worker_handle: tokio::task::JoinHandle<IoResult<()>>,
}

#[derive(Clone)]
pub struct InProcessAppServerRequestHandle {
    command_tx: mpsc::Sender<ClientCommand>,
}

#[derive(Clone)]
pub enum AppServerRequestHandle {
    InProcess(InProcessAppServerRequestHandle),
    Remote(crate::remote::RemoteAppServerRequestHandle),
}

pub enum AppServerClient {
    InProcess(InProcessAppServerClient),
    Remote(RemoteAppServerClient),
}

impl InProcessAppServerClient {
    /// Starts the in-process runtime and facade worker task.
    ///
    /// The returned client is ready for requests and event consumption. If the
    /// internal event queue is saturated later, server requests are rejected
    /// with overload error instead of being silently dropped.
    pub async fn start(args: InProcessClientStartArgs) -> IoResult<Self> {
        let channel_capacity = args.channel_capacity.max(1);
        let mut handle =
            codex_app_server::in_process::start(args.into_runtime_start_args()).await?;
        let initialize_response = handle
            .initialize_response()
            .cloned()
            .map(AppServerInitializeResponse::from_json)
            .ok_or_else(|| IoError::new(ErrorKind::InvalidData, "missing initialize response"))?;
        let request_sender = handle.sender();
        let (command_tx, mut command_rx) = mpsc::channel::<ClientCommand>(channel_capacity);
        let (event_tx, event_rx) = mpsc::channel::<InProcessServerEvent>(channel_capacity);

        let worker_handle = tokio::spawn(async move {
            let mut shutdown_ack = None;
            let mut skipped_events = 0usize;
            let mut pending_delivery = std::collections::VecDeque::new();
            loop {
                tokio::select! {
                    // A retained request handle cannot keep an input-capable
                    // runtime alive after its only event consumer has gone away.
                    _ = event_tx.closed() => break,
                    permit = event_tx.reserve(), if !pending_delivery.is_empty() => {
                        match permit {
                            Ok(permit) => {
                                if let Some(event) = pending_delivery.pop_front() {
                                    permit.send(event);
                                }
                            }
                            Err(_) => {
                                break;
                            }
                        }
                    }
                    command = command_rx.recv() => {
                        match command {
                            Some(command) if command.is_abandoned_request() => {}
                            Some(ClientCommand::Request { request, trace, response_tx }) => {
                                let request_sender = request_sender.clone();
                                // Request waits happen on a detached task so
                                // this loop can keep draining runtime events
                                // while the request is blocked on client input.
                                tokio::spawn(async move {
                                    let result = request_sender.request_with_trace(*request, trace).await;
                                    let _ = response_tx.send(result);
                                });
                            }
                            Some(ClientCommand::Notify {
                                notification,
                                response_tx,
                            }) => {
                                let result = request_sender.notify(notification);
                                let _ = response_tx.send(result);
                            }
                            Some(ClientCommand::ResolveServerRequest {
                                request_id,
                                result,
                                response_tx,
                            }) => {
                                let send_result =
                                    request_sender.respond_to_server_request(request_id, result);
                                let _ = response_tx.send(send_result);
                            }
                            Some(ClientCommand::RejectServerRequest {
                                request_id,
                                error,
                                response_tx,
                            }) => {
                                let send_result = request_sender.fail_server_request(request_id, error);
                                let _ = response_tx.send(send_result);
                            }
                            Some(ClientCommand::Shutdown { response_tx }) => {
                                shutdown_ack = Some(response_tx);
                                break;
                            }
                            None => {
                                break;
                            }
                        }
                    }
                    event = handle.next_event(), if pending_delivery.is_empty() => {
                        let Some(event) = event else {
                            break;
                        };
                        if let InProcessServerEvent::ServerRequest(
                            ServerRequest::ChatgptAuthTokensRefresh { request_id, .. }
                        ) = &event
                        {
                            let send_result = request_sender.fail_server_request(
                                request_id.clone(),
                                JSONRPCErrorError {
                                    code: -32000,
                                    message: "chatgpt auth token refresh is not supported for in-process app-server clients".to_string(),
                                    data: None,
                                },
                            );
                            if let Err(err) = send_result {
                                warn!(
                                    "failed to reject unsupported chatgpt auth token refresh request: {err}"
                                );
                            }
                            continue;
                        }

                        match forward_in_process_event(
                            &event_tx,
                            &mut pending_delivery,
                            &mut skipped_events,
                            event,
                            |request| {
                                let _ = request_sender.fail_server_request(
                                    request.id().clone(),
                                    codex_app_server_protocol::overloaded_error(
                                        codex_app_server_protocol::OverloadReason::InProcessAppServerEventQueue,
                                        "in-process app-server event queue is full",
                                    ),
                                );
                            },
                        ) {
                            ForwardEventResult::Continue => {
                            }
                            ForwardEventResult::DisableStream => {
                                break;
                            }
                        }
                    }
                }
            }
            // Retire facade admission before joining the existing runtime owner.
            // Closing unread runtime events fails outstanding interactions and
            // preserves custody of committed work through graceful shutdown.
            command_rx.close();
            drop(command_rx);
            let result = handle.shutdown().await;
            if let Some(response_tx) = shutdown_ack {
                let _ = response_tx.send(result.as_ref().map(|_| ()).map_err(|error| {
                    IoError::new(error.kind(), error.to_string())
                }));
            }
            result
        });

        Ok(Self {
            command_tx,
            event_rx,
            initialize_response,
            worker_handle,
        })
    }

    pub fn initialize_response(&self) -> &AppServerInitializeResponse {
        &self.initialize_response
    }

    pub fn request_handle(&self) -> InProcessAppServerRequestHandle {
        InProcessAppServerRequestHandle {
            command_tx: self.command_tx.clone(),
        }
    }

    /// Sends a typed client request and returns raw JSON-RPC result.
    ///
    /// Callers that expect a concrete response type should usually prefer
    /// [`request_typed`](Self::request_typed).
    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        self.request_handle().request(request).await
    }

    /// Sends a typed client request and decodes the successful response body.
    ///
    /// This still deserializes from a JSON value produced by app-server's
    /// JSON-RPC result envelope. Because the caller chooses `T`, `Deserialize`
    /// failures indicate an internal request/response mismatch at the call site
    /// (or an in-process bug), not transport skew from an external client.
    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        self.request_handle().request_typed(request).await
    }

    /// Sends a typed client notification.
    pub async fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::Notify {
                notification,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server notify channel is closed",
            )
        })?
    }

    /// Resolves a pending server request.
    ///
    /// This should only be called with request IDs obtained from the current
    /// client's event stream.
    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::ResolveServerRequest {
                request_id,
                result,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server resolve channel is closed",
            )
        })?
    }

    /// Rejects a pending server request with JSON-RPC error payload.
    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::RejectServerRequest {
                request_id,
                error,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server reject channel is closed",
            )
        })?
    }

    /// Returns the next in-process event, or `None` when worker exits.
    ///
    /// Callers are expected to drain this stream promptly. If they fall behind,
    /// the worker emits [`InProcessServerEvent::Lagged`] markers and may reject
    /// pending server requests rather than letting approval flows hang.
    pub async fn next_event(&mut self) -> Option<InProcessServerEvent> {
        self.event_rx.recv().await
    }

    /// Shuts down worker and in-process runtime with bounded wait.
    ///
    /// Timeout stops observation, not custody of committed runtime work.
    pub async fn shutdown(self) -> IoResult<()> {
        let owner = tokio::spawn(self.shutdown_owned());
        tokio::time::timeout(SHUTDOWN_TIMEOUT, owner)
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::TimedOut,
                    "in-process shutdown remains owned and pending",
                )
            })?
            .map_err(|error| IoError::other(format!("in-process shutdown owner failed: {error}")))?
    }

    async fn shutdown_owned(self) -> IoResult<()> {
        let Self {
            command_tx,
            event_rx,
            initialize_response: _initialize_response,
            worker_handle,
        } = self;
        // Drop the caller-facing receiver before asking the worker to shut
        // down. That unblocks any pending must-deliver `event_tx.send(..)`
        // so the worker can reach `handle.shutdown()` instead of timing out
        // and getting aborted with the runtime still attached.
        drop(event_rx);
        let (response_tx, response_rx) = oneshot::channel();
        // Only the public observer has a deadline. This retained owner keeps
        // shutdown intent alive even while command admission is blocked.
        if command_tx
            .send(ClientCommand::Shutdown { response_tx })
            .await
            .is_ok()
        {
            let _ = response_rx.await;
        }
        drop(command_tx);
        // Event-consumer closure may beat the queued command. The joined worker
        // result still confirms runtime cleanup (or propagates its real failure).
        worker_handle
            .await
            .map_err(|error| IoError::other(format!("in-process facade worker failed: {error}")))?
    }
}

impl InProcessAppServerRequestHandle {
    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::Request {
                request: Box::new(request),
                trace: codex_otel::current_span_w3c_trace_context(),
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server request channel is closed",
            )
        })?
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        let method = request.method_name().to_string();
        let response =
            self.request(request)
                .await
                .map_err(|source| TypedRequestError::Transport {
                    method: method.clone(),
                    source,
                })?;
        let result = response.map_err(|source| TypedRequestError::Server {
            method: method.clone(),
            source,
        })?;
        serde_json::from_value(result)
            .map_err(|source| TypedRequestError::Deserialize { method, source })
    }
}

impl AppServerRequestHandle {
    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        match self {
            Self::InProcess(handle) => handle.request(request).await,
            Self::Remote(handle) => handle.request(request).await,
        }
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        match self {
            Self::InProcess(handle) => handle.request_typed(request).await,
            Self::Remote(handle) => handle.request_typed(request).await,
        }
    }
}

impl AppServerClient {
    pub fn initialize_response(&self) -> &AppServerInitializeResponse {
        match self {
            Self::InProcess(client) => client.initialize_response(),
            Self::Remote(client) => client.initialize_response(),
        }
    }

    pub fn codex_home(&self, local_codex_home: &AbsolutePathBuf) -> Option<AppServerPath> {
        match self {
            Self::InProcess(_) => Some(AppServerPath::from_app_server(
                local_codex_home.display().to_string(),
            )),
            Self::Remote(client) => client.codex_home().map(AppServerPath::from_app_server),
        }
    }

    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        self.request_handle().request(request).await
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        self.request_handle().request_typed(request).await
    }

    pub async fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        match self {
            Self::InProcess(client) => client.notify(notification).await,
            Self::Remote(client) => client.notify(notification).await,
        }
    }

    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        match self {
            Self::InProcess(client) => client.resolve_server_request(request_id, result).await,
            Self::Remote(client) => client.resolve_server_request(request_id, result).await,
        }
    }

    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        match self {
            Self::InProcess(client) => client.reject_server_request(request_id, error).await,
            Self::Remote(client) => client.reject_server_request(request_id, error).await,
        }
    }

    pub async fn next_event(&mut self) -> Option<AppServerEvent> {
        match self {
            Self::InProcess(client) => client.next_event().await.map(Into::into),
            Self::Remote(client) => client.next_event().await,
        }
    }

    pub async fn shutdown(self) -> IoResult<()> {
        match self {
            Self::InProcess(client) => client.shutdown().await,
            Self::Remote(client) => client.shutdown().await,
        }
    }

    pub fn request_handle(&self) -> AppServerRequestHandle {
        match self {
            Self::InProcess(client) => AppServerRequestHandle::InProcess(client.request_handle()),
            Self::Remote(client) => AppServerRequestHandle::Remote(client.request_handle()),
        }
    }
}

