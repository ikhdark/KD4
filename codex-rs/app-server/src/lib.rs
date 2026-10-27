#![recursion_limit = "256"]
#![deny(clippy::print_stdout, clippy::print_stderr)]

use codex_arg0::Arg0DispatchPaths;
use codex_config::ConfigLayerStackOrdering;
use codex_config::LoaderOverrides;
use codex_config::NoopThreadConfigLoader;
use codex_config::thread_config_loader_for_endpoint;
use codex_core::config::Config;
use codex_core::resolve_installation_id;
use codex_login::AuthConfig;
use codex_login::AuthManager;
use codex_login::default_client::set_default_client_residency_requirement;
#[cfg(debug_assertions)]
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cli::CliConfigOverrides;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::analytics_utils::analytics_events_client_from_config;
use crate::config_manager::ConfigManager;
use crate::connection_cleanup::ConnectionCleanupTasks;
use crate::message_processor::MessageProcessor;
use crate::message_processor::MessageProcessorArgs;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::QueuedOutgoingMessage;
use crate::thread_state::ConnectionCapabilities;
use crate::transport::CHANNEL_CAPACITY;
use crate::transport::ConnectionOrigin;
use crate::transport::ConnectionState;
use crate::transport::OutboundConnectionState;
use crate::transport::RemoteControlPolicy;
use crate::transport::RemoteControlStartConfig;
use crate::transport::TransportEvent;
use crate::transport::acquire_app_server_startup_lock;
use crate::transport::app_server_startup_lock_path;
use crate::transport::auth::policy_from_settings;
use crate::transport::prepare_control_socket_path;
use crate::transport::route_outgoing_envelope;
use crate::transport::start_control_socket_acceptor;
use crate::transport::start_remote_control;
use crate::transport::start_stdio_connection;
use crate::transport::start_websocket_acceptor;
use crate::transport::validate_websocket_listener;
use codex_analytics::AppServerRpcTransport;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::RemoteControlStatusChangedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::TextPosition as AppTextPosition;
use codex_app_server_protocol::TextRange as AppTextRange;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLoadError;
use codex_config::TextRange as CoreTextRange;
use codex_core::ExecPolicyError;
use codex_core::check_execpolicy_for_warnings;
use codex_core::config::find_codex_home;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::SessionSource;
use codex_rollout::state_integration as rollout_state_integration;
use codex_state::log_db;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;
use tracing::warn;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::Registry;
use tracing_subscriber::util::SubscriberInitExt;

const SQLITE_RECOVERY_CONFIG_WARNING_SUMMARY: &str =
    "Codex rebuilt a corrupt local database; database-only data may be missing.";
const STARTUP_WARNING_TARGET: &str = "codex_app_server::startup_warning";
const DEFAULT_STDERR_LOG_FILTER: &str = "warn";

fn default_stderr_log_filter() -> EnvFilter {
    EnvFilter::new(DEFAULT_STDERR_LOG_FILTER)
}

fn stderr_log_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| default_stderr_log_filter())
}

fn emit_startup_warning(warning: &ConfigWarningNotification) {
    match &warning.details {
        Some(details) => warn!(target: STARTUP_WARNING_TARGET, "{} {}", warning.summary, details),
        None => warn!(target: STARTUP_WARNING_TARGET, "{}", warning.summary),
    }
}

mod analytics_utils;
mod app_info;
mod app_server_tracing;
mod attestation;
mod bespoke_event_handling;
mod build_info;
mod command_exec;
mod config_layer;
mod config_manager;
mod config_manager_service;
mod connection_cleanup;
mod connection_rpc_gate;
mod current_time;
mod dynamic_tools;
mod error_code;
mod extensions;
mod external_auth;
mod filters;
mod fs_watch;
mod fuzzy_file_search;
mod image_url;
pub mod in_process;
mod mcp_refresh;
mod message_processor;
mod models;
mod models_refresh_worker;
mod outgoing_message;
mod request_processors;
mod request_serialization;
mod runtime_provenance;
mod server_request_error;
mod skills_watcher;
mod thread_state;
mod thread_status;
mod transport;

pub use crate::error_code::INPUT_TOO_LARGE_ERROR_CODE;
pub use crate::error_code::INVALID_PARAMS_ERROR_CODE;
pub use crate::transport::AppServerTransport;
pub use crate::transport::RemoteControlStartupMode;
pub use crate::transport::app_server_control_socket_path;
pub use crate::transport::auth::AppServerWebsocketAuthArgs;
pub use crate::transport::auth::AppServerWebsocketAuthSettings;
pub use crate::transport::auth::WebsocketAuthCliMode;
pub use crate::transport::take_remote_control_disabled_env;

const LOG_FORMAT_ENV_VAR: &str = "LOG_FORMAT";
const OTEL_SERVICE_NAME: &str = "codex-app-server";
#[cfg(debug_assertions)]
const TEST_USER_CONFIG_FILE_ENV_VAR: &str = "CODEX_APP_SERVER_TEST_USER_CONFIG_FILE";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogFormat {
    Default,
    Json,
}

type StderrLogLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

async fn install_config_loaders_from_config(
    config_manager: &ConfigManager,
    config: &Config,
) -> Arc<AuthManager> {
    let discovered_thread_config_loader =
        thread_config_loader_for_endpoint(config.experimental_thread_config_endpoint.as_deref());
    config_manager.replace_thread_config_loader(Arc::clone(&discovered_thread_config_loader));
    let auth_manager =
        AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ false).await;
    config_manager.replace_cloud_config_bundle_loader(
        Arc::clone(&auth_manager),
        config.chatgpt_base_url.clone(),
        config.http_client_factory(),
    );
    auth_manager
}

async fn finish_startup_config_loaders(
    config_manager: &ConfigManager,
    config: &Config,
    bootstrap: Option<(Config, Arc<AuthManager>)>,
) -> Arc<AuthManager> {
    if let Some((previous, auth_manager)) = bootstrap
        && AuthConfig::from_config(&previous) == AuthConfig::from_config(config)
        && previous.experimental_thread_config_endpoint
            == config.experimental_thread_config_endpoint
    {
        // The managed-config pass already consumed these loaders. Keep its completed
        // bundle and auth owner rather than reloading credentials and spawning another
        // startup fetch/refresher. This handoff exists only during process startup;
        // account changes still explicitly replace the loaders.
        return auth_manager;
    }
    install_config_loaders_from_config(config_manager, config).await
}

/// Control-plane messages from the processor/transport side to the outbound router task.
///
/// `run_main` uses two loops/tasks:
/// - processor loop: handles incoming JSON-RPC and request dispatch
/// - outbound loop: performs potentially slow writes to per-connection writers
///
/// `OutboundControlEvent` keeps those loops coordinated without sharing mutable
/// connection state directly. In particular, the outbound loop needs to know
/// when a connection opens/closes so it can route messages correctly.
enum OutboundControlEvent {
    /// Register a new writer for an opened connection.
    Opened {
        connection_id: ConnectionId,
        state: OutboundConnectionState,
    },
    /// Remove state for a closed/disconnected connection.
    Closed { connection_id: ConnectionId },
    /// Disconnect all connection-oriented clients during graceful restart.
    DisconnectAll,
}

fn prepare_connection(
    connection_id: ConnectionId,
    origin: transport::ConnectionOrigin,
    writer: mpsc::Sender<QueuedOutgoingMessage>,
    disconnect_sender: Option<CancellationToken>,
    transport_shutdown: CancellationToken,
) -> (ConnectionState, OutboundControlEvent) {
    let mut connection = ConnectionState::new(
        origin,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(transport::OutboundNotificationOptOuts::new(HashSet::new())),
    );
    connection.disconnect = disconnect_sender.clone().unwrap_or_else(|| transport_shutdown.clone());
    let state = OutboundConnectionState::new(
        writer,
        Arc::clone(&connection.outbound_initialized),
        Arc::clone(&connection.outbound_experimental_api_enabled),
        Arc::clone(&connection.outbound_opted_out_notification_methods),
        disconnect_sender,
        transport_shutdown,
    );
    let state = if matches!(origin, ConnectionOrigin::Stdio) {
        state.with_stdio_delivery()
    } else {
        state
    };
    (
        connection,
        OutboundControlEvent::Opened {
            connection_id,
            state,
        },
    )
}

async fn process_connection_request(
    processor: &Arc<MessageProcessor>,
    outgoing: &Arc<OutgoingMessageSender>,
    connection_id: ConnectionId,
    request: JSONRPCRequest,
    transport: &AppServerTransport,
    connection: &ConnectionState,
    remote_status_rx: &tokio::sync::watch::Receiver<RemoteControlStatusChangedNotification>,
    remote_status_publication: &tokio::sync::Mutex<()>,
) {
    let was_initialized = connection.session.initialized();
    processor
        .process_request(connection_id, request, transport, Arc::clone(&connection.session))
        .await;
    if !was_initialized && connection.session.initialized() {
        let experimental_api_enabled = connection.session.experimental_api_enabled();
        if !connection.outbound_opted_out_notification_methods.replace(
            connection.session.opted_out_notification_methods(),
        ) {
            warn!("failed to update outbound opted-out notifications");
        }
        connection.outbound_experimental_api_enabled
            .store(experimental_api_enabled, std::sync::atomic::Ordering::Release);
        processor.initialize_processor
            .send_initialize_notifications_to_connection(connection_id).await;
        processor.thread_processor.thread_state_manager.connection_initialized(
            connection_id,
            ConnectionCapabilities {
                request_attestation: connection.session.request_attestation(),
                experimental_api: experimental_api_enabled,
            },
        ).await;
        // Serialize the initial snapshot and readiness with changed-status sends.
        // Sample only after acquiring publication ownership: a value captured
        // before initialization can otherwise overwrite a newer broadcast.
        let _publication = remote_status_publication.lock().await;
        let remote_status = remote_status_rx.borrow().clone();
        outgoing.send_server_notification_to_connections(
            &[connection_id], ServerNotification::RemoteControlStatusChanged(remote_status),
        ).await;
        connection.outbound_initialized.store(true, std::sync::atomic::Ordering::Release);
    }
}

#[derive(Default)]
struct ShutdownState {
    requested: bool,
    forced: bool,
    signal_listener_failed: bool,
    deadline: Option<tokio::time::Instant>,
    last_logged_running_turn_count: Option<usize>,
}

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

enum ShutdownAction {
    Noop,
    Finish,
}

#[derive(Clone, Copy)]
enum ShutdownSignal {
    Forceable,
}

async fn shutdown_signal() -> IoResult<ShutdownSignal> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
        Ok(ShutdownSignal::Forceable)
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .map(|_| ShutdownSignal::Forceable)
}

async fn startup_shutdown_signal(signal: impl std::future::Future<Output = IoResult<()>>) {
    if let Err(error) = signal.await {
        tracing::warn!(%error, "startup shutdown signal listener failed; continuing admission without it");
        std::future::pending::<()>().await;
    }
}


impl ShutdownState {
    fn requested(&self) -> bool {
        self.requested
    }

    fn forced(&self) -> bool {
        self.forced
    }

    fn on_signal(
        &mut self,
        signal: ShutdownSignal,
        connection_count: usize,
        running_turn_count: usize,
    ) {
        if self.requested {
            if matches!(signal, ShutdownSignal::Forceable) {
                self.forced = true;
            }
            return;
        }

        self.requested = true;
        self.deadline = Some(tokio::time::Instant::now() + SHUTDOWN_TIMEOUT);
        self.last_logged_running_turn_count = None;
        info!(
            "received shutdown signal; entering graceful restart drain (connections={}, runningAssistantTurns={}, new work rejected; completion controls remain available)",
            connection_count, running_turn_count,
        );
    }

    fn update(&mut self, running_turn_count: usize, connection_count: usize) -> ShutdownAction {
        if !self.requested {
            return ShutdownAction::Noop;
        }

        if self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            self.forced = true;
            warn!("app-server shared shutdown deadline expired; forcing cleanup");
        }
        if self.forced || running_turn_count == 0 {
            if self.forced {
                info!(
                    "forcing restart with {running_turn_count} running assistant turn(s) and {connection_count} connection(s)"
                );
            } else {
                info!(
                    "shutdown signal restart: no assistant turns running; stopping acceptor and disconnecting {connection_count} connection(s)"
                );
            }
            return ShutdownAction::Finish;
        }

        if self.last_logged_running_turn_count != Some(running_turn_count) {
            info!(
                "shutdown signal restart: waiting for {running_turn_count} running assistant turn(s) to finish"
            );
            self.last_logged_running_turn_count = Some(running_turn_count);
        }

        ShutdownAction::Noop
    }

    async fn finish_cleanup(
        &mut self,
        cleanup: impl std::future::Future<Output = ()>,
        signal: impl std::future::Future<Output = IoResult<ShutdownSignal>>,
    ) {
        if self.forced {
            return;
        }
        let deadline = *self
            .deadline
            .get_or_insert_with(|| tokio::time::Instant::now() + SHUTDOWN_TIMEOUT);
        tokio::pin!(cleanup);
        tokio::pin!(signal);
        tokio::select! {
            biased;
            result = &mut signal, if !self.signal_listener_failed => {
                match result {
                    Ok(ShutdownSignal::Forceable) => {
                        self.forced = true;
                        warn!("shutdown signal during cleanup; forcing restart");
                    }
                    Err(error) => {
                        self.signal_listener_failed = true;
                        warn!(%error, "shutdown signal listener failed during cleanup; retaining shared deadline");
                        self.forced = tokio::time::timeout_at(deadline, &mut cleanup).await.is_err();
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                self.forced = true;
                warn!("app-server shared shutdown deadline expired during cleanup");
            }
            _ = &mut cleanup => {}
        }
    }
}

fn config_warning_from_error(
    summary: impl Into<String>,
    err: &std::io::Error,
) -> ConfigWarningNotification {
    let (path, range) = match config_error_location(err) {
        Some((path, range)) => (Some(path), Some(range)),
        None => (None, None),
    };
    ConfigWarningNotification {
        summary: summary.into(),
        details: Some(err.to_string()),
        path,
        range,
    }
}

fn config_error_location(err: &std::io::Error) -> Option<(String, AppTextRange)> {
    err.get_ref()
        .and_then(|err| err.downcast_ref::<ConfigLoadError>())
        .map(|err| {
            let config_error = err.config_error();
            (
                config_error.path.to_string_lossy().to_string(),
                app_text_range(&config_error.range),
            )
        })
}

fn exec_policy_warning_location(err: &ExecPolicyError) -> (Option<String>, Option<AppTextRange>) {
    match err {
        ExecPolicyError::ParsePolicy { path, source } => {
            if let Some(location) = source.location() {
                return (Some(location.path), Some(app_text_range(&location.range)));
            }
            (Some(path.clone()), None)
        }
        _ => (None, None),
    }
}

fn exec_policy_config_warning(err: &ExecPolicyError) -> ConfigWarningNotification {
    let (path, range) = exec_policy_warning_location(err);
    ConfigWarningNotification {
        summary: "Error parsing rules; custom rules not applied.".to_string(),
        details: Some(err.to_string()),
        path,
        range,
    }
}

fn app_text_range(range: &CoreTextRange) -> AppTextRange {
    AppTextRange {
        start: AppTextPosition {
            line: range.start.line,
            column: range.start.column,
        },
        end: AppTextPosition {
            line: range.end.line,
            column: range.end.column,
        },
    }
}

fn project_config_warning(config: &Config) -> Option<ConfigWarningNotification> {
    let mut disabled_folders = Vec::new();

    for layer in config.config_layer_stack.get_layers(
        ConfigLayerStackOrdering::LowestPrecedenceFirst,
        /*include_disabled*/ true,
    ) {
        let ConfigLayerSource::Project { dot_codex_folder } = &layer.name else {
            continue;
        };
        let Some(disabled_reason) = &layer.disabled_reason else {
            continue;
        };
        disabled_folders.push((
            dot_codex_folder.as_path().display().to_string(),
            disabled_reason.clone(),
        ));
    }

    if disabled_folders.is_empty() {
        return None;
    }

    let mut message = concat!(
        "Project-local config, hooks, and exec policies are disabled in the following folders ",
        "until the project is trusted, but skills still load.\n",
    )
    .to_string();
    for (index, (folder, reason)) in disabled_folders.iter().enumerate() {
        let display_index = index + 1;
        let _ = writeln!(message, "    {display_index}. {folder}");
        let _ = writeln!(message, "       {reason}");
    }

    Some(ConfigWarningNotification {
        summary: message,
        details: None,
        path: None,
        range: None,
    })
}

impl LogFormat {
    fn from_env_value(value: Option<&str>) -> Self {
        match value.map(str::trim).map(str::to_ascii_lowercase) {
            Some(value) if value == "json" => Self::Json,
            _ => Self::Default,
        }
    }
}

fn log_format_from_env() -> LogFormat {
    let value = std::env::var(LOG_FORMAT_ENV_VAR).ok();
    LogFormat::from_env_value(value.as_deref())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStartupTasks {
    Start,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppServerRuntimeOptions {
    pub plugin_startup_tasks: PluginStartupTasks,
    pub remote_control_startup_mode: RemoteControlStartupMode,
    pub install_shutdown_signal_handler: bool,
}

impl Default for AppServerRuntimeOptions {
    fn default() -> Self {
        Self {
            plugin_startup_tasks: PluginStartupTasks::Start,
            remote_control_startup_mode: RemoteControlStartupMode::ResolvePersisted,
            install_shutdown_signal_handler: true,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_main(
    arg0_paths: Arg0DispatchPaths,
    cli_config_overrides: CliConfigOverrides,
    loader_overrides: LoaderOverrides,
    strict_config: bool,
    default_analytics_enabled: bool,
    transport: AppServerTransport,
    session_source: SessionSource,
    auth: AppServerWebsocketAuthSettings,
    runtime_options: AppServerRuntimeOptions,
) -> IoResult<()> {
    let loader_overrides = loader_overrides_with_test_user_config_file(
        loader_overrides,
        test_user_config_file_from_env(),
    )?;
    let (transport_event_tx, mut transport_event_rx) =
        mpsc::channel::<TransportEvent>(codex_app_server_transport::INGRESS_CAPACITY);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<OutgoingEnvelope>(CHANNEL_CAPACITY);
    let (outbound_control_tx, mut outbound_control_rx) =
        mpsc::channel::<OutboundControlEvent>(CHANNEL_CAPACITY);

    // Parse CLI overrides once and derive the base Config eagerly so later
    // components do not need to work with raw TOML values.
    let cli_kv_overrides = cli_config_overrides.parse_overrides().map_err(|e| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("error parsing -c overrides: {e}"),
        )
    })?;
    let codex_home = find_codex_home()?;
    let ignore_user_config = loader_overrides.ignore_user_config;
    let config_manager = ConfigManager::new(
        codex_home.to_path_buf(),
        cli_kv_overrides.clone(),
        loader_overrides,
        strict_config,
        Default::default(),
        arg0_paths.clone(),
        Arc::new(NoopThreadConfigLoader),
    );
    let mut config_warnings = Vec::new();
    let bootstrap = match config_manager
        .load_latest_config(/*fallback_cwd*/ None)
        .await
    {
        Ok(config) => {
            let auth_manager = install_config_loaders_from_config(&config_manager, &config).await;
            Some((config, auth_manager))
        }
        Err(err) => {
            warn!(
                error = %err,
                "failed to preload startup config; managed config loaders will use the effective startup configuration if startup continues"
            );
            // Non-strict startup reports the configuration error below and falls back to
            // defaults. The effective startup config still installs loader handles before
            // request processing starts, so a preload failure does not leave the manager
            // permanently on the bootstrap no-op loaders.
            None
        }
    };
    let (config, auth_manager, config_resolved) = match config_manager
        .load_latest_config(/*fallback_cwd*/ None)
        .await
    {
        Ok(config) => {
            let auth_manager =
                finish_startup_config_loaders(&config_manager, &config, bootstrap).await;
            (config, auth_manager, true)
        }
        Err(err) => {
            if strict_config {
                return Err(err);
            }

            drop(bootstrap);
            let message = config_warning_from_error("Invalid configuration; using defaults.", &err);
            config_warnings.push(message);
            let default_config = config_manager.load_default_config().await.map_err(|e| {
                std::io::Error::new(
                    ErrorKind::InvalidData,
                    format!("error loading default config after config error: {e}"),
                )
            })?;
            let auth_manager =
                install_config_loaders_from_config(&config_manager, &default_config).await;
            (default_config, auth_manager, false)
        }
    };

    set_default_client_residency_requirement(config.enforce_residency.value());

    // Defaults support configuration repair, not permission to bypass unresolved policy.
    let remote_control_policy = if !config_resolved
        || config
            .config_layer_stack
            .requirements()
            .allow_remote_control
            .as_ref()
            .is_some_and(|requirement| !requirement.value)
    {
        RemoteControlPolicy::DisabledByRequirements
    } else {
        RemoteControlPolicy::Allowed
    };
    let remote_control_startup_mode = runtime_options.remote_control_startup_mode;
    let remote_control_explicitly_requested =
        remote_control_startup_mode == RemoteControlStartupMode::EnabledEphemeral;
    if remote_control_explicitly_requested
        && remote_control_policy == RemoteControlPolicy::DisabledByRequirements
    {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "remote control is disabled by managed requirements",
        ));
    }

    let mut websocket_auth_policy = match &transport {
        AppServerTransport::WebSocket { bind_address } => {
            let policy = policy_from_settings(&auth)?;
            validate_websocket_listener(*bind_address, &policy)?;
            Some(policy)
        }
        _ => None,
    };

    let local_runtime_paths =
        ExecServerRuntimePaths::from_optional_path(arg0_paths.codex_self_exe.clone())?;
    let environment_manager = if ignore_user_config {
        EnvironmentManager::from_env_with_http_client_factory(Some(local_runtime_paths), config.http_client_factory()).await
    } else {
        EnvironmentManager::from_codex_home_with_http_client_factory(codex_home.clone(), Some(local_runtime_paths), config.http_client_factory()).await
    }
    .map(Arc::new)
    .map_err(std::io::Error::other)?;

    let otel = codex_core::otel_init::build_provider(
        &config,
        env!("CARGO_PKG_VERSION"),
        Some(OTEL_SERVICE_NAME),
        default_analytics_enabled,
    )
    .map_err(|e| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!("error loading otel config: {e}"),
        )
    })?;
    codex_core::otel_init::record_process_start(otel.as_ref(), OTEL_SERVICE_NAME);
    codex_core::otel_init::install_sqlite_telemetry(otel.as_ref(), OTEL_SERVICE_NAME);
    // Admission shares one budget, with signal supervision already active while
    // waiting. Dropping any wait releases its guards; no native lock wait survives.
    let startup_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let startup_signal = async {
        if runtime_options.install_shutdown_signal_handler {
            startup_shutdown_signal(tokio::signal::ctrl_c()).await
        } else {
            std::future::pending::<()>().await
        }
    };
    tokio::pin!(startup_signal);
    let unix_socket_startup_lock = match &transport {
        AppServerTransport::UnixSocket { socket_path } => {
            let startup_lock_path = app_server_startup_lock_path(&codex_home)?;
            let startup_lock = tokio::select! {
                result = tokio::time::timeout_at(startup_deadline, acquire_app_server_startup_lock(startup_lock_path)) =>
                    result.map_err(|_| std::io::Error::new(ErrorKind::TimedOut, "app-server startup admission timed out"))??,
                _ = &mut startup_signal => return Err(std::io::Error::new(ErrorKind::Interrupted, "app-server startup cancelled")),
            };
            prepare_control_socket_path(socket_path.as_path()).await?;
            Some(startup_lock)
        }
        _ => None,
    };
    let state_db_init = match init_sqlite_state_db_with_fresh_start_on_corruption(&config).await {
        Ok(state_db_init) => state_db_init,
        Err(err) => {
            return Err(std::io::Error::other(format!(
                "failed to initialize sqlite state runtime under {}: {err:#}",
                config.sqlite_home.display()
            )));
        }
    };
    let state_db = state_db_init.state_db;
    if let Some(recovery_notice) = state_db_init.recovery_notice {
        config_warnings.push(ConfigWarningNotification {
            summary: SQLITE_RECOVERY_CONFIG_WARNING_SUMMARY.to_string(),
            details: Some(recovery_notice.details),
            path: None,
            range: None,
        });
    }

    if let Ok(Some(err)) = check_execpolicy_for_warnings(&config.config_layer_stack).await {
        config_warnings.push(exec_policy_config_warning(&err));
    }

    if let Some(warning) = project_config_warning(&config) {
        config_warnings.push(warning);
    }
    for warning in &config.startup_warnings {
        config_warnings.push(ConfigWarningNotification {
            summary: warning.clone(),
            details: None,
            path: None,
            range: None,
        });
    }

    let feedback = CodexFeedback::new();

    // Install a simple subscriber so `tracing` output is visible. Users can
    // control the log level with `RUST_LOG` and switch to JSON logs with
    // `LOG_FORMAT=json`.
    // SQLx enters the caller's span for every worker command. Avoid synchronous
    // stderr writes on entry/exit while that worker may hold a write transaction.
    // Keep span boundaries, busy/idle timings, and explicit events.
    let stderr_fmt: StderrLogLayer = match log_format_from_env() {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_writer(std::io::stderr)
            .with_span_events(
                tracing_subscriber::fmt::format::FmtSpan::NEW
                    | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
            )
            .with_filter(stderr_log_filter())
            .boxed(),
        LogFormat::Default => tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_span_events(
                tracing_subscriber::fmt::format::FmtSpan::NEW
                    | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
            )
            .with_filter(stderr_log_filter())
            .boxed(),
    };

    let feedback_layer = feedback.logger_layer();
    let feedback_metadata_layer = feedback.metadata_layer();
    let log_db = state_db.clone().map(log_db::start);
    let log_db_layer = log_db
        .clone()
        .map(|layer| layer.with_filter(log_db::default_filter()));
    let otel_logger_layer = otel.as_ref().and_then(|o| o.logger_layer());
    let otel_tracing_layer = otel.as_ref().and_then(|o| o.tracing_layer());
    let _ = tracing_subscriber::registry()
        .with(stderr_fmt)
        .with(feedback_layer)
        .with(feedback_metadata_layer)
        .with(log_db_layer)
        .with(otel_logger_layer)
        .with(otel_tracing_layer)
        .try_init();
    for warning in &config_warnings {
        emit_startup_warning(warning);
    }
    let installation_id = tokio::select! {
        result = tokio::time::timeout_at(startup_deadline, resolve_installation_id(&config.codex_home)) =>
            result.map_err(|_| std::io::Error::new(ErrorKind::TimedOut, "app-server identity admission timed out"))??,
        _ = &mut startup_signal => return Err(std::io::Error::new(ErrorKind::Interrupted, "app-server startup cancelled")),
    };
    let transport_shutdown_token = CancellationToken::new();
    let force_shutdown_token = CancellationToken::new();
    let (shutdown_observation_tx, shutdown_observation_rx) =
        oneshot::channel::<futures::future::BoxFuture<'static, ()>>();
    let mut transport_accept_handles = Vec::<JoinHandle<()>>::new();

    let single_client_mode = matches!(&transport, AppServerTransport::Stdio);
    let shutdown_when_no_connections = single_client_mode;
    let graceful_signal_restart_enabled =
        runtime_options.install_shutdown_signal_handler && !single_client_mode;
    let mut app_server_client_name_rx = None;

    match &transport {
        AppServerTransport::Stdio => {
            let (stdio_client_name_tx, stdio_client_name_rx) = oneshot::channel::<String>();
            app_server_client_name_rx = Some(stdio_client_name_rx);
            start_stdio_connection(
                transport_event_tx.clone(),
                &mut transport_accept_handles,
                stdio_client_name_tx,
            )
            .await?;
        }
        AppServerTransport::UnixSocket { socket_path } => {
            let accept = start_control_socket_acceptor(
                socket_path.clone(),
                transport_event_tx.clone(),
                transport_shutdown_token.clone(),
            );
            let accept_handle = tokio::select! {
                result = tokio::time::timeout_at(startup_deadline, accept) =>
                    result.map_err(|_| std::io::Error::new(ErrorKind::TimedOut, "app-server endpoint admission timed out"))??,
                _ = &mut startup_signal => return Err(std::io::Error::new(ErrorKind::Interrupted, "app-server startup cancelled")),
            };
            transport_accept_handles.push(accept_handle);
        }
        AppServerTransport::WebSocket { bind_address } => {
            let accept_handle = start_websocket_acceptor(
                *bind_address,
                transport_event_tx.clone(),
                transport_shutdown_token.clone(),
                websocket_auth_policy.take().ok_or_else(|| {
                    std::io::Error::other(
                        "websocket auth policy was not resolved during startup preflight",
                    )
                })?,
            )
            .await?;
            transport_accept_handles.push(accept_handle);
        }
        AppServerTransport::Off => {}
    }
    drop(unix_socket_startup_lock);

    let remote_control_enabled = remote_control_policy == RemoteControlPolicy::Allowed
        && remote_control_explicitly_requested
        && state_db.is_some();
    if remote_control_explicitly_requested && state_db.is_none() {
        error!("remote control disabled because sqlite state db is unavailable");
    }
    let no_local_transport = transport_accept_handles.is_empty();
    if no_local_transport
        && remote_control_startup_mode != RemoteControlStartupMode::ResolvePersisted
        && !remote_control_enabled
    {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            if remote_control_policy == RemoteControlPolicy::DisabledByRequirements {
                "no transport configured; remote control disabled by managed requirements"
            } else if remote_control_explicitly_requested && state_db.is_none() {
                "no transport configured; remote control disabled because sqlite state db is unavailable"
            } else {
                "no transport configured; use --listen or enable remote control"
            },
        ));
    }

    let (remote_control_accept_handle, remote_control_handle) = start_remote_control(
        RemoteControlStartConfig {
            remote_control_url: config.chatgpt_base_url.clone(),
            installation_id: installation_id.clone(),
            policy: remote_control_policy,
            http_client_factory: config.http_client_factory(),
        },
        state_db.clone(),
        auth_manager.clone(),
        transport_event_tx.clone(),
        transport_shutdown_token.clone(),
        app_server_client_name_rx,
        remote_control_startup_mode,
    )
    .await?;
    if no_local_transport
        && remote_control_startup_mode == RemoteControlStartupMode::ResolvePersisted
    {
        let persisted_enabled = match remote_control_handle
            .resolve_persisted_preference(/*app_server_client_name*/ None)
            .await
        {
            Ok(persisted_enabled) => persisted_enabled,
            Err(err) => {
                warn!("failed to resolve persisted remote control preference: {err}");
                false
            }
        };
        if !persisted_enabled {
            transport_shutdown_token.cancel();
            let _ = remote_control_accept_handle.await;
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                if remote_control_policy == RemoteControlPolicy::DisabledByRequirements {
                    "no transport configured; remote control disabled by managed requirements"
                } else {
                    "no transport configured; use --listen or enable remote control"
                },
            ));
        }
    }
    config_manager.bind_remote_control_handle(remote_control_handle.clone());
    transport_accept_handles.push(remote_control_accept_handle);
    transport_accept_handles.push(tokio::spawn(
        config_manager
            .clone()
            .recheck_requirements_after_identity_completion(
                Arc::clone(&auth_manager),
                config.chatgpt_base_url.clone(),
                config.http_client_factory(),
                transport_shutdown_token.clone(),
            ),
    ));

    let outbound_handle = tokio::spawn(async move {
        let mut outbound_connections = HashMap::<ConnectionId, OutboundConnectionState>::new();
        loop {
            outbound_connections.retain(|_, state| !state.is_disconnected());
            tokio::select! {
                    biased;
                    event = outbound_control_rx.recv() => {
                        let Some(event) = event else {
                            break;
                        };
                        match event {
                            OutboundControlEvent::Opened {
                                connection_id,
                                state,
                            } => {
                                outbound_connections.insert(connection_id, state);
                            }
                            OutboundControlEvent::Closed { connection_id } => {
                                outbound_connections.remove(&connection_id);
                            }
                            OutboundControlEvent::DisconnectAll => {
                                info!(
                                    "disconnecting {} outbound websocket connection(s) for graceful restart",
                                    outbound_connections.len()
                                );
                                for connection_state in outbound_connections.values() {
                                    connection_state.request_disconnect();
                                }
                                outbound_connections.clear();
                            }
                        }
                    }
                    envelope = outgoing_rx.recv() => {
                    let Some(envelope) = envelope else {
                        break;
                    };
                    route_outgoing_envelope(&mut outbound_connections, envelope).await;
                }
            }
        }
        info!("outbound router task exited (channel closed)");
    });

    let processor_handle = tokio::spawn({
        let auth_manager = Arc::clone(&auth_manager);
        let analytics_events_client =
            analytics_events_client_from_config(Arc::clone(&auth_manager), &config);
        let outgoing_message_sender = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            analytics_events_client.clone(),
        ));
        let initialize_notification_sender = outgoing_message_sender.clone();
        let outbound_control_tx = outbound_control_tx;
        let processor = Arc::new(MessageProcessor::new(MessageProcessorArgs {
            outgoing: outgoing_message_sender,
            analytics_events_client,
            arg0_paths,
            config: Arc::new(config),
            config_manager,
            environment_manager,
            feedback: feedback.clone(),
            log_db,
            state_db: state_db.clone(),
            config_warnings,
            session_source,
            auth_manager,
            installation_id,
            rpc_transport: analytics_rpc_transport(&transport),
            remote_control_handle: Some(remote_control_handle.clone()),
            plugin_startup_tasks: runtime_options.plugin_startup_tasks,
        }));
        let mut thread_created_rx = processor.thread_manager.subscribe_thread_created();
        let mut running_turn_count_rx = processor
            .thread_processor
            .thread_watch_manager
            .subscribe_running_turn_count();
        let mut connections = HashMap::<ConnectionId, Arc<ConnectionState>>::new();
        let mut connection_cleanup_tasks = ConnectionCleanupTasks::new();
        // Initialization watchdogs share `connection_cleanup_tasks`; release them
        // when the loop exits so shutdown cleanup does not wait out their timer.
        let init_watchdog_shutdown = CancellationToken::new();
        let remote_control_status_rx = remote_control_handle.status_receiver();
        let remote_status_publication = Arc::new(tokio::sync::Mutex::new(()));
        {
            // Watch coalesces status changes while the bounded output owner is
            // congested. No status send can hold the shared ingress dispatcher.
            let mut status_rx = remote_control_status_rx.clone();
            let outgoing = Arc::clone(&initialize_notification_sender);
            let stop = init_watchdog_shutdown.clone();
            let publication = Arc::clone(&remote_status_publication);
            connection_cleanup_tasks.spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        changed = status_rx.changed() => if changed.is_err() { break; },
                    }
                    let _publication = tokio::select! {
                        _ = stop.cancelled() => break,
                        guard = publication.lock() => guard,
                    };
                    let status = status_rx.borrow_and_update().clone();
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = outgoing.send_server_notification(ServerNotification::RemoteControlStatusChanged(status)) => {},
                    }
                }
            });
        }
        let transport_shutdown_token = transport_shutdown_token.clone();
        let force_shutdown_token = force_shutdown_token.clone();
        async move {
            let mut listen_for_threads = true;
            // One ordered lifecycle worker; the bounded broadcast coalesces
            // overflow into the existing lag/resync path instead of blocking ingress.
            let mut thread_attachment_tasks = tokio::task::JoinSet::new();
            let mut shutdown_state = ShutdownState::default();
            // Keep the registered listener across unrelated select branches so a pending
            // SIGTERM is not lost when transport traffic wins the race.
            let mut shutdown_signal_wait = Box::pin(shutdown_signal());
            let exit_reason = loop {
                let running_turn_count = {
                    let running_turn_count = running_turn_count_rx.borrow();
                    *running_turn_count
                };
                if matches!(
                    shutdown_state.update(running_turn_count, connections.len()),
                    ShutdownAction::Finish
                ) {
                    break "shutdown_requested";
                }

                tokio::select! {
                    _ = transport_shutdown_token.cancelled() => break "outbound_delivery_failed",
                    _ = tokio::time::sleep_until(shutdown_state.deadline.unwrap_or_else(|| tokio::time::Instant::now() + SHUTDOWN_TIMEOUT)), if shutdown_state.requested() => {
                        shutdown_state.forced = true;
                    }
                    shutdown_signal_result = &mut shutdown_signal_wait, if graceful_signal_restart_enabled && !shutdown_state.forced() => {
                        shutdown_signal_wait.set(shutdown_signal());
                        let signal = match shutdown_signal_result {
                            Ok(signal) => signal,
                            Err(err) => {
                                warn!("failed to listen for shutdown signal during graceful restart drain: {err}");
                                continue;
                            }
                        };
                        let running_turn_count = *running_turn_count_rx.borrow();
                        shutdown_state.on_signal(signal, connections.len(), running_turn_count);
                        processor.begin_drain();
                    }
                    changed = running_turn_count_rx.changed(), if graceful_signal_restart_enabled && shutdown_state.requested() => {
                        if changed.is_err() {
                            warn!("running-turn watcher closed during graceful restart drain");
                        }
                    }
                    event = transport_event_rx.recv() => {
                        let Some(event) = event else {
                            break "transport_channel_closed";
                        };
                        match event {
                            TransportEvent::ConnectionOpened {
                                connection_id,
                                origin,
                                writer,
                                disconnect_sender,
                            } => {
                                let (mut connection, outbound_opened) = prepare_connection(
                                    connection_id,
                                    origin,
                                    writer,
                                    disconnect_sender.clone(),
                                    transport_shutdown_token.clone(),
                                );
                                if matches!(origin, ConnectionOrigin::WebSocket)
                                    && let Some(disconnect) = disconnect_sender.as_ref()
                                {
                                    let disconnect = disconnect.clone();
                                    let initialized = Arc::clone(&connection.outbound_initialized);
                                    let watchdog_shutdown = init_watchdog_shutdown.clone();
                                    connection_cleanup_tasks.spawn(async move {
                                        tokio::select! {
                                            _ = disconnect.cancelled() => {},
                                            _ = watchdog_shutdown.cancelled() => {},
                                            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                                                if !initialized.load(std::sync::atomic::Ordering::Acquire) { disconnect.cancel(); }
                                            }
                                        }
                                    });
                                }
                                match outbound_control_tx.try_send(outbound_opened) {
                                    Ok(()) => {},
                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                        // Dropping the unopened outbound state retires only
                                        // this connection; never await a congested router.
                                        connection.disconnect.cancel();
                                        continue;
                                    }
                                    Err(mpsc::error::TrySendError::Closed(_)) => break "outbound_router_closed",
                                }
                                let (ingress, mut requests) = transport::ConnectionRequestIngress::channel();
                                connection.request_ingress = Some(ingress);
                                let connection = Arc::new(connection);
                                let worker_connection = Arc::clone(&connection);
                                let worker_processor = Arc::clone(&processor);
                                let outgoing = Arc::clone(&initialize_notification_sender);
                                let worker_transport = transport.clone();
                                let status_rx = remote_control_status_rx.clone();
                                let status_publication = Arc::clone(&remote_status_publication);
                                connection_cleanup_tasks.spawn(async move {
                                    let cancelled = worker_connection.session.rpc_gate.cancellation_token();
                                    tokio::select! {
                                        biased;
                                        _ = cancelled.cancelled() => return,
                                        _ = outgoing.connection_opened_with_runtime(
                                            connection_id,
                                            origin,
                                            Arc::clone(&worker_connection.outbound_initialized),
                                            worker_connection.disconnect.clone(),
                                        ) => {},
                                    }
                                    loop {
                                        let next = tokio::select! {
                                            biased;
                                            _ = cancelled.cancelled() => break,
                                            next = requests.recv() => next,
                                        };
                                        let Some(queued) = next else { break; };
                                        tokio::select! {
                                            biased;
                                            _ = cancelled.cancelled() => break,
                                            _ = process_connection_request(
                                                &worker_processor, &outgoing, connection_id,
                                                queued.request, &worker_transport, &worker_connection,
                                                &status_rx, &status_publication,
                                            ) => {},
                                        }
                                        drop(queued._credit);
                                    }
                                });
                                connections.insert(connection_id, connection);
                            }
                            TransportEvent::ConnectionClosed { connection_id } => {
                                let Some(connection_state) = connections.remove(&connection_id) else {
                                    continue;
                                };
                                connection_state.session.rpc_gate.close().await;
                                connection_state.disconnect.cancel();
                                let outbound_closed = !matches!(outbound_control_tx.try_send(
                                    OutboundControlEvent::Closed { connection_id },
                                ), Err(mpsc::error::TrySendError::Closed(_)));
                                let processor = Arc::clone(&processor);
                                connection_cleanup_tasks.spawn(async move {
                                    processor
                                        .connection_closed(connection_id, &connection_state.session)
                                        .await;
                                });
                                if !outbound_closed {
                                    break "outbound_router_closed";
                                }
                                if shutdown_when_no_connections && connections.is_empty() {
                                    break "last_connection_closed";
                                }
                            }
                            TransportEvent::IncomingMessage { connection_id, message } => {
                                match message {
                                    JSONRPCMessage::Request(request) => {
                                        let Some(connection) = connections.get(&connection_id) else {
                                            warn!("dropping request from unknown connection: {connection_id:?}");
                                            continue;
                                        };
                                        if !connection.request_ingress.as_ref().is_some_and(|ingress| ingress.try_send(request)) {
                                            warn!(?connection_id, "retiring overloaded connection request ingress");
                                            connection.disconnect.cancel();
                                        }
                                    }
                                    JSONRPCMessage::Response(response) => {
                                        if !connections.contains_key(&connection_id) {
                                            warn!("dropping response from unknown connection: {connection_id:?}");
                                            continue;
                                        }
                                        processor.process_response(connection_id, response).await;
                                    }
                                    JSONRPCMessage::Notification(notification) => {
                                        if !connections.contains_key(&connection_id) {
                                            warn!("dropping notification from unknown connection: {connection_id:?}");
                                            continue;
                                        }
                                        processor
                                            .process_notification(connection_id, notification)
                                            .await;
                                    }
                                    JSONRPCMessage::Error(err) => {
                                        if !connections.contains_key(&connection_id) {
                                            warn!("dropping error from unknown connection: {connection_id:?}");
                                            continue;
                                        }
                                        processor.process_error(connection_id, err).await;
                                    }
                                }
                            }
                        }
                    }
                    _ = connection_cleanup_tasks.reap_next() => {}
                    result = thread_attachment_tasks.join_next(), if !thread_attachment_tasks.is_empty() => {
                        match result {
                            Some(Ok(keep_listening)) => listen_for_threads = keep_listening,
                            Some(Err(error)) => warn!(%error, "thread listener attachment worker failed"),
                            None => {}
                        }
                    }
                    created = thread_created_rx.recv(), if listen_for_threads && thread_attachment_tasks.is_empty() => {
                        let initialized_connection_ids = connections
                            .iter()
                            .filter_map(|(connection_id, connection_state)| {
                                connection_state
                                    .session
                                    .initialized()
                                    .then_some(*connection_id)
                            })
                            .collect();
                        let processor = Arc::clone(&processor);
                        thread_attachment_tasks.spawn(async move {
                            processor.thread_processor
                                .handle_thread_created_event(created, initialized_connection_ids).await
                        });
                    }
                }
            };

            init_watchdog_shutdown.cancel();
            thread_attachment_tasks.shutdown().await;
            let disconnect_all = shutdown_state.requested();
            shutdown_state
                .finish_cleanup(
                    async {
                        processor.account_processor.cancel_active_login().await;
                        futures::future::join_all(
                            connections.values().map(|connection_state| {
                                connection_state.session.rpc_gate.shutdown()
                            }),
                        )
                        .await;
                        connection_cleanup_tasks.drain().await;
                        processor.drain_background_tasks().await;
                        processor.thread_processor.shutdown_threads().await;
                        processor.thread_processor.wait_for_background_tasks().await;
                        processor.finish_shutdown_delivery().await;
                        if disconnect_all {
                            let _ = outbound_control_tx
                                .send(OutboundControlEvent::DisconnectAll)
                                .await;
                            transport_shutdown_token.cancel();
                        }
                    },
                    async {
                        if graceful_signal_restart_enabled {
                            shutdown_signal_wait.as_mut().await
                        } else {
                            std::future::pending().await
                        }
                    },
                )
                .await;
            if shutdown_state.forced() {
                connection_cleanup_tasks.abort();
                transport_shutdown_token.cancel();
                force_shutdown_token.cancel();
            }
            info!(
                exit_reason,
                remaining_connection_count = connections.len(),
                shutdown_forced = shutdown_state.forced(),
                "processor task exited"
            );
            // Transfer the same listener and remaining budget, rather than
            // ending supervision while outbound/transport owners still drain.
            let _ = shutdown_observation_tx.send(Box::pin(async move {
                shutdown_state
                    .finish_cleanup(std::future::pending(), async {
                        if graceful_signal_restart_enabled {
                            shutdown_signal_wait.await
                        } else {
                            std::future::pending().await
                        }
                    })
                    .await;
            }));
        }
    });

    drop(transport_event_tx);

    let task_result = finish_runtime_task_groups(
        processor_handle,
        outbound_handle,
        transport_shutdown_token,
        transport_accept_handles,
        force_shutdown_token,
        async {
            match shutdown_observation_rx.await {
                Ok(observation) => observation.await,
                // Processor failure already has its own failure-drain bound.
                Err(_) => std::future::pending().await,
            }
        },
    )
    .await;

    if let Some(otel) = otel {
        otel.shutdown();
    }

    task_result
}

async fn finish_runtime_task_groups(
    processor: JoinHandle<()>,
    outbound: JoinHandle<()>,
    transport_shutdown: CancellationToken,
    transport_handles: Vec<JoinHandle<()>>,
    force_shutdown: CancellationToken,
    shutdown_observation: impl std::future::Future<Output = ()>,
) -> IoResult<()> {
    use futures::StreamExt;
    use futures::stream::FuturesUnordered;

    let mut owners = FuturesUnordered::new();
    let mut abort_handles = Vec::new();
    for (name, handle) in [("processor", processor), ("outbound router", outbound)]
        .into_iter()
        .chain(
            transport_handles
                .into_iter()
                .map(|handle| ("transport", handle)),
        )
    {
        abort_handles.push(handle.abort_handle());
        owners.push(async move { (name, handle.await) });
    }
    let mut first_error = None;
    let mut primary_owners = 2;
    let mut failure_deadline = None;
    let mut forced = false;
    tokio::pin!(shutdown_observation);
    while !owners.is_empty() {
        let next_owner = async {
            if let Some(deadline) = failure_deadline {
                match tokio::time::timeout_at(deadline, owners.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        for handle in &abort_handles {
                            handle.abort();
                        }
                        failure_deadline = None;
                        owners.next().await
                    }
                }
            } else {
                owners.next().await
            }
        };
        let next = tokio::select! {
            biased;
            _ = async {
                tokio::select! {
                    _ = force_shutdown.cancelled() => {}
                    _ = &mut shutdown_observation => {}
                }
            }, if !forced => {
                forced = true;
                transport_shutdown.cancel();
                for handle in &abort_handles {
                    handle.abort();
                }
                continue;
            }
            next = next_owner => next,
        };
        let Some((task, result)) = next else { break };
        if task != "transport" {
            primary_owners -= 1;
        }
        if let Err(error) = result {
            if forced && error.is_cancelled() {
                continue;
            }
            warn!(task, %error, "app-server runtime task failed");
            if first_error.is_none() {
                first_error = Some(std::io::Error::other(format!(
                    "app-server {task} task failed: {error}"
                )));
                failure_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(10));
            }
            transport_shutdown.cancel();
        } else if primary_owners == 0 {
            // Healthy processor shutdown still drains outbound terminal output.
            transport_shutdown.cancel();
        }
    }
    // Retire all task owners before exposing an infrastructure failure to the
    // caller. run_main also shuts down telemetry before propagating this result.
    first_error.map_or(Ok(()), Err)
}

struct SqliteRecoveryNotice {
    details: String,
}

struct RecoveredSqliteDatabase {
    database_path: String,
    backup_folder: String,
}

struct StateDbInitResult {
    state_db: Option<rollout_state_integration::StateDbHandle>,
    recovery_notice: Option<SqliteRecoveryNotice>,
}

async fn init_sqlite_state_db_with_fresh_start_on_corruption(
    config: &Config,
) -> anyhow::Result<StateDbInitResult> {
    let mut attempted_backups = HashSet::new();
    let mut recovered_databases = Vec::new();
    loop {
        let err = match rollout_state_integration::try_init(config).await {
            Ok(state_db) => {
                let recovery_notice = sqlite_recovery_notice(&recovered_databases);
                if recovery_notice.is_some() {
                    emit_state_db_backup_warning(SQLITE_RECOVERY_CONFIG_WARNING_SUMMARY);
                    for recovered_database in &recovered_databases {
                        emit_state_db_backup_warning(&format!(
                            "Database path: {}",
                            recovered_database.database_path
                        ));
                        emit_state_db_backup_warning(&format!(
                            "Backup folder: {}",
                            recovered_database.backup_folder
                        ));
                    }
                }
                return Ok(StateDbInitResult {
                    state_db: Some(state_db),
                    recovery_notice,
                });
            }
            Err(err) => err,
        };
        let database_path = codex_state::runtime_db_path_for_corruption_error(&err)
            .unwrap_or_else(|| codex_state::state_db_path(config.sqlite_home.as_path()));
        if !codex_state::is_sqlite_corruption_error(&err)
            && !sqlite_home_is_blocking_file(database_path.as_path())
        {
            return Err(err);
        }

        if !attempted_backups.insert(database_path.clone()) {
            return Err(anyhow::anyhow!(
                "failed to initialize sqlite state runtime after moving damaged database file into a backup folder: {err}"
            ));
        }

        let original_error = err.to_string();
        emit_state_db_backup_warning(&format!(
            "Codex local database at {} appears damaged. Moving it into a backup folder so the app server can rebuild it from saved data.",
            database_path.display()
        ));
        let backups = codex_state::StateRuntime::recover_for_fresh_start(
            database_path.clone(), config.model_provider_id.clone(),
        )
            .await
            .map_err(|backup_err| {
                anyhow::anyhow!(
                    "failed to move damaged sqlite state database files into a backup folder: {backup_err}; original error: {original_error}"
                )
            })?;
        for backup in &backups {
            emit_state_db_backup_warning(&format!(
                "Moved damaged Codex local database file {} to {}",
                backup.original_path.display(),
                backup.backup_path.display()
            ));
        }
        if let Some(first_backup) = backups.first()
            && let Some(backup_folder) = first_backup.backup_path.parent()
        {
            recovered_databases.push(RecoveredSqliteDatabase {
                database_path: first_backup.original_path.display().to_string(),
                backup_folder: backup_folder.display().to_string(),
            });
        }
    }
}

fn sqlite_home_is_blocking_file(database_path: &Path) -> bool {
    database_path
        .parent()
        .and_then(|path| std::fs::metadata(path).ok())
        .is_some_and(|metadata| metadata.is_file())
}

fn sqlite_recovery_notice(
    recovered_databases: &[RecoveredSqliteDatabase],
) -> Option<SqliteRecoveryNotice> {
    if recovered_databases.is_empty() {
        return None;
    }

    let details = recovered_databases
        .iter()
        .map(|recovered_database| {
            format!(
                "Database path: {}\nBackup folder: {}\n\
                 The corrupt database was backed up and replaced. Session rollout files were \
                 not deleted. Thread metadata recoverable from those files is rebuilt; \
                 data stored only in this database may be missing.",
                recovered_database.database_path, recovered_database.backup_folder
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(SqliteRecoveryNotice { details })
}

fn emit_state_db_backup_warning(message: &str) {
    warn!("{message}");
    if !tracing::dispatcher::has_been_set() {
        #[allow(clippy::print_stderr)]
        {
            eprintln!("{message}");
        }
    }
}

fn test_user_config_file_from_env() -> Option<std::path::PathBuf> {
    #[cfg(debug_assertions)]
    {
        std::env::var_os(TEST_USER_CONFIG_FILE_ENV_VAR)
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
    }

    #[cfg(not(debug_assertions))]
    None
}

fn loader_overrides_with_test_user_config_file(
    loader_overrides: LoaderOverrides,
    test_user_config_file: Option<std::path::PathBuf>,
) -> IoResult<LoaderOverrides> {
    #[cfg(debug_assertions)]
    let mut loader_overrides = loader_overrides;

    #[cfg(debug_assertions)]
    if let Some(path) = test_user_config_file {
        let path = AbsolutePathBuf::from_absolute_path(path).map_err(|err| {
            std::io::Error::new(
                ErrorKind::InvalidInput,
                format!("invalid test user config path: {err}"),
            )
        })?;
        warn!(
            path = %path.as_path().display(),
            "using debug-only app-server test user config file"
        );
        loader_overrides.user_config_path = Some(path);
    }

    #[cfg(not(debug_assertions))]
    let _ = test_user_config_file;

    Ok(loader_overrides)
}

fn analytics_rpc_transport(transport: &AppServerTransport) -> AppServerRpcTransport {
    match transport {
        AppServerTransport::Stdio => AppServerRpcTransport::Stdio,
        AppServerTransport::UnixSocket { .. }
        | AppServerTransport::WebSocket { .. }
        | AppServerTransport::Off => AppServerRpcTransport::Websocket,
    }
}


