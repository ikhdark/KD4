//! In-process app-server runtime host for local embedders.
//!
//! This module runs the existing [`MessageProcessor`] and outbound routing logic
//! on Tokio tasks, but replaces socket/stdio transports with bounded in-memory
//! channels. The intent is to preserve app-server semantics while avoiding a
//! process boundary for CLI surfaces that run in the same process.
//!
//! # Lifecycle
//!
//! 1. Construct runtime state with [`InProcessStartArgs`].
//! 2. Call [`start`], which performs the `initialize` / `initialized` handshake
//!    internally and returns a ready-to-use [`InProcessClientHandle`].
//! 3. Obtain an [`InProcessClientSender`] from [`InProcessClientHandle::sender`]
//!    for requests and notifications, and consume events via
//!    [`InProcessClientHandle::next_event`].
//! 4. Terminate with [`InProcessClientHandle::shutdown`].
//!
//! # Transport model
//!
//! The runtime is transport-local but not protocol-free. Incoming requests are
//! typed [`ClientRequest`] values, yet responses still come back through the
//! same JSON-RPC result envelope that `MessageProcessor` uses for stdio and
//! websocket transports. This keeps in-process behavior aligned with
//! app-server rather than creating a second execution contract.
//!
//! # Backpressure
//!
//! Command submission uses `try_send` and can return `WouldBlock`. Transcript
//! and terminal notifications wait for event-channel capacity, while explicitly
//! best-effort notifications may be dropped under saturation. Server requests
//! are never silently abandoned: if they cannot be queued they are failed back
//! into `MessageProcessor` with overload or internal errors so approval flows do
//! not hang indefinitely.
//!
//! # Relationship to `codex-app-server-client`
//!
//! This module provides the low-level runtime handle ([`InProcessClientHandle`]).
//! Higher-level callers (TUI, exec) should go through `codex-app-server-client`,
//! which wraps this module behind a worker task with async request/response
//! helpers, surface-specific startup policy, and bounded shutdown.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::analytics_utils::analytics_events_client_from_config;
use crate::config_manager::ConfigManager;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use codex_app_server_rpc::connection_session::ConnectionSessionState;
use codex_app_server_handlers::MessageProcessor;
use codex_app_server_handlers::MessageProcessorArgs;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::QueuedOutgoingMessage;
use crate::transport::CHANNEL_CAPACITY;
use crate::transport::OutboundConnectionState;
use crate::transport::route_outgoing_envelope;
use codex_analytics::AppServerRpcTransport;
use codex_app_server_protocol_types::ClientNotification;
use codex_app_server_protocol_types::ClientRequest;
use codex_app_server_protocol_types::ConfigWarningNotification;
use codex_app_server_protocol_types::InitializeParams;
use codex_app_server_protocol_types::JSONRPCErrorError;
use codex_app_server_protocol_types::OverloadReason;
use codex_app_server_protocol_types::RequestId;
use codex_app_server_protocol_types::Result;
use codex_app_server_protocol_types::ServerNotification;
use codex_app_server_protocol_types::ServerRequest;
use codex_app_server_protocol_types::overloaded_error;
use codex_app_server_protocol_types::server_notification_requires_delivery;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::ThreadConfigLoader;
use codex_core::check_execpolicy_for_warnings;
use codex_core::config::Config;
use codex_core::resolve_installation_id;
use codex_exec_server::EnvironmentManager;
use codex_feedback::CodexFeedback;
use codex_login::AuthManager;
use codex_protocol::protocol::SessionSource;
pub use codex_rollout::StateDbHandle;
pub use codex_state::log_db::LogDbLayer;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use toml::Value as TomlValue;
use tracing::warn;

const IN_PROCESS_CONNECTION_ID: ConnectionId = ConnectionId(0);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_REQUEST_CAPACITY: usize = 8;
/// Default bounded channel capacity for in-process runtime queues.
pub const DEFAULT_IN_PROCESS_CHANNEL_CAPACITY: usize = CHANNEL_CAPACITY;

type PendingClientRequestResponse = std::result::Result<Result, JSONRPCErrorError>;

enum ServerRequestDeliveryError {
    Full(Box<ServerRequest>),
    Closed(Box<ServerRequest>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NotificationDelivery {
    Delivered,
    Dropped,
    Closed,
}

fn in_process_request_is_control(request: &ClientRequest) -> bool {
    request.is_control()
        && codex_app_server_transport::serialized_request_bytes_up_to(request, 64 * 1024)
            <= 64 * 1024
}

async fn forward_server_notification(
    event_tx: &mpsc::Sender<InProcessServerEvent>,
    skipped_events: &mut usize,
    notification: ServerNotification,
) -> NotificationDelivery {
    let requires_delivery = server_notification_requires_delivery(&notification);
    if requires_delivery {
        if *skipped_events > 0 {
            if event_tx
                .send(InProcessServerEvent::Lagged { skipped: *skipped_events })
                .await
                .is_err()
            {
                return NotificationDelivery::Closed;
            }
            *skipped_events = 0;
        }
        return if event_tx
            .send(InProcessServerEvent::ServerNotification(notification))
            .await
            .is_ok()
        {
            NotificationDelivery::Delivered
        } else {
            NotificationDelivery::Closed
        };
    }

    // Reserve the actual event first. An advisory marker must not permanently
    // consume the only slot after one transient drop.
    let permit = match event_tx.try_reserve() {
        Ok(permit) => permit,
        Err(mpsc::error::TrySendError::Full(_)) => {
            if *skipped_events == 0 {
                warn!("dropping in-process server notifications (queue full)");
            }
            *skipped_events = skipped_events.saturating_add(1);
            return NotificationDelivery::Dropped;
        }
        Err(mpsc::error::TrySendError::Closed(_)) => return NotificationDelivery::Closed,
    };
    if *skipped_events > 0 {
        match event_tx.try_send(InProcessServerEvent::Lagged { skipped: *skipped_events }) {
            Ok(()) => *skipped_events = 0,
            Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => return NotificationDelivery::Closed,
        }
    }
    permit.send(InProcessServerEvent::ServerNotification(notification));
    NotificationDelivery::Delivered
}

fn forward_server_request(
    event_tx: &mpsc::Sender<InProcessServerEvent>,
    skipped_events: &mut usize,
    request: ServerRequest,
) -> std::result::Result<(), ServerRequestDeliveryError> {
    // Reserve the actionable request's slot before attempting advisory lag
    // delivery. Capacity one is supported: keep the lag count for a later
    // event rather than letting the marker manufacture a request rejection.
    let permit = match event_tx.try_reserve() {
        Ok(permit) => permit,
        Err(mpsc::error::TrySendError::Full(_)) => {
            *skipped_events = skipped_events.saturating_add(1);
            return Err(ServerRequestDeliveryError::Full(Box::new(request)));
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            return Err(ServerRequestDeliveryError::Closed(Box::new(request)));
        }
    };
    if *skipped_events > 0 {
        match event_tx.try_send(InProcessServerEvent::Lagged {
            skipped: *skipped_events,
        }) {
            Ok(()) => *skipped_events = 0,
            Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(ServerRequestDeliveryError::Closed(Box::new(request)));
            }
        }
    }

    permit.send(InProcessServerEvent::ServerRequest(request));
    Ok(())
}

/// Input needed to start an in-process app-server runtime.
///
/// These fields mirror the pieces of ambient process state that stdio and
/// websocket transports normally assemble before `MessageProcessor` starts.
#[derive(Clone)]
pub struct InProcessStartArgs {
    /// Resolved argv0 dispatch paths used by command execution internals.
    pub arg0_paths: Arg0DispatchPaths,
    /// Shared base config used to initialize core components.
    pub config: Arc<Config>,
    /// CLI config overrides that are already parsed into TOML values.
    pub cli_overrides: Vec<(String, TomlValue)>,
    /// Loader override knobs used by config API paths.
    pub loader_overrides: LoaderOverrides,
    /// Whether config API paths should reject unknown config fields.
    pub strict_config: bool,
    /// Preloaded cloud config bundle provider.
    pub cloud_config_bundle: CloudConfigBundleLoader,
    /// Loader used to fetch typed thread config sources before a thread starts.
    pub thread_config_loader: Arc<dyn ThreadConfigLoader>,
    /// Feedback sink used by app-server/core telemetry and logs.
    pub feedback: CodexFeedback,
    /// SQLite tracing layer used to flush recently emitted logs before feedback upload.
    pub log_db: Option<LogDbLayer>,
    /// Process-wide SQLite state handle shared with embedded app-server consumers.
    pub state_db: Option<StateDbHandle>,
    /// Environment manager used by core execution and filesystem operations.
    pub environment_manager: Arc<EnvironmentManager>,
    /// Startup warnings emitted after initialize succeeds.
    pub config_warnings: Vec<ConfigWarningNotification>,
    /// Session source stamped into thread/session metadata.
    pub session_source: SessionSource,
    /// Whether auth loading should honor the `CODEX_API_KEY` environment variable.
    pub enable_codex_api_key_env: bool,
    /// Initialize params used for initial handshake.
    pub initialize: InitializeParams,
    /// Capacity used for runtime queues and dispatched requests awaiting replies
    /// (clamped to at least 1). Excess requests receive a retryable overload error.
    pub channel_capacity: usize,
}

/// Event emitted from the app-server to the in-process client.
///
/// [`Lagged`](Self::Lagged) is a transport health marker, not an application
/// event — it signals that the consumer fell behind and some events were dropped.
#[derive(Debug, Clone)]
// Keep the public in-process event shape aligned with the typed transport API.
#[allow(clippy::large_enum_variant)]
pub enum InProcessServerEvent {
    /// Server request that requires client response/rejection.
    ServerRequest(ServerRequest),
    /// App-server notification directed to the embedded client.
    ServerNotification(ServerNotification),
    /// Indicates one or more events were dropped due to backpressure.
    Lagged { skipped: usize },
}

/// Internal message sent from [`InProcessClientHandle`] methods to the runtime task.
///
/// Requests carry a oneshot sender for the response; notifications and server-request
/// replies are fire-and-forget from the caller's perspective (transport errors are
/// caught by `try_send` on the outer channel).
enum InProcessClientMessage {
    Request {
        request: Box<ClientRequest>,
        trace: Option<codex_protocol::protocol::W3cTraceContext>,
        response_tx: oneshot::Sender<PendingClientRequestResponse>,
    },
    Notification {
        notification: ClientNotification,
    },
    ServerRequestResponse {
        request_id: RequestId,
        result: Result,
    },
    ServerRequestError {
        request_id: RequestId,
        error: JSONRPCErrorError,
    },
    Shutdown {
        done_tx: oneshot::Sender<IoResult<()>>,
    },
}

enum ProcessorCommand {
    Request {
        request: Box<ClientRequest>,
        trace: Option<codex_protocol::protocol::W3cTraceContext>,
    },
    Notification(ClientNotification),
}

#[derive(Clone)]
pub struct InProcessClientSender {
    client_tx: mpsc::Sender<InProcessClientMessage>,
}

impl InProcessClientSender {
    /// Sends a typed client request into the in-process runtime.
    ///
    /// The returned value is a transport-level `IoResult` containing either a
    /// JSON-RPC success payload or JSON-RPC error payload. Callers must keep
    /// request IDs unique among concurrent requests; reusing an in-flight ID
    /// produces an `INVALID_REQUEST` response and can make request routing
    /// ambiguous in the caller.
    pub async fn request(&self, request: ClientRequest) -> IoResult<PendingClientRequestResponse> {
        self.request_with_trace(request, codex_otel::current_span_w3c_trace_context())
            .await
    }

    /// Preserve an explicitly captured caller carrier across facade worker hops.
    pub async fn request_with_trace(
        &self,
        request: ClientRequest,
        trace: Option<codex_protocol::protocol::W3cTraceContext>,
    ) -> IoResult<PendingClientRequestResponse> {
        let (response_tx, response_rx) = oneshot::channel();
        self.try_send_client_message(InProcessClientMessage::Request {
            request: Box::new(request),
            trace,
            response_tx,
        })?;
        response_rx.await.map_err(|err| {
            IoError::new(
                ErrorKind::BrokenPipe,
                format!("in-process request response channel closed: {err}"),
            )
        })
    }

    /// Sends a typed client notification into the in-process runtime.
    ///
    /// Notifications do not have an application-level response. Transport
    /// errors indicate queue saturation or closed runtime.
    pub fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        self.try_send_client_message(InProcessClientMessage::Notification { notification })
    }

    /// Resolves a pending [`ServerRequest`](InProcessServerEvent::ServerRequest).
    ///
    /// This should be used only with request IDs received from the current
    /// runtime event stream; sending arbitrary IDs has no effect on app-server
    /// state and can mask a stuck approval flow in the caller.
    pub fn respond_to_server_request(&self, request_id: RequestId, result: Result) -> IoResult<()> {
        self.try_send_client_message(InProcessClientMessage::ServerRequestResponse {
            request_id,
            result,
        })
    }

    /// Rejects a pending [`ServerRequest`](InProcessServerEvent::ServerRequest).
    ///
    /// Use this when the embedder cannot satisfy a server request; leaving
    /// requests unanswered can stall turn progress.
    pub fn fail_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        self.try_send_client_message(InProcessClientMessage::ServerRequestError {
            request_id,
            error,
        })
    }

    fn try_send_client_message(&self, message: InProcessClientMessage) -> IoResult<()> {
        let control = match &message {
            InProcessClientMessage::Request { request, .. } => {
                in_process_request_is_control(request)
            }
            // Client replies and lifecycle messages also need ingress progress.
            _ => true,
        };
        match codex_app_server_transport::try_send_with_control_reserve(
            &self.client_tx,
            message,
            control,
            CONTROL_REQUEST_CAPACITY,
        ) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(IoError::new(
                ErrorKind::WouldBlock,
                "in-process app-server client queue is full",
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server runtime is closed",
            )),
        }
    }
}

/// Handle used by an in-process client to call app-server and consume events.
///
/// This is the low-level runtime handle. Higher-level callers should usually go
/// through `codex-app-server-client`, which adds worker-task buffering,
/// request/response helpers, and surface-specific startup policy.
pub struct InProcessClientHandle {
    observation_id: u64,
    client: InProcessClientSender,
    event_rx: mpsc::Receiver<InProcessServerEvent>,
    runtime_handle: tokio::task::JoinHandle<IoResult<()>>,
    initialize_response: Option<codex_app_server_protocol_types::Result>,
}

impl InProcessClientHandle {
    /// Receives the next server event from the in-process runtime.
    ///
    /// Returns `None` when the runtime task exits and no more events are
    /// available.
    pub async fn next_event(&mut self) -> Option<InProcessServerEvent> {
        self.event_rx.recv().await
    }

    /// Requests runtime shutdown and waits for worker termination.
    ///
    /// The deadline bounds observation. Committed cleanup retains its runtime
    /// owner even if this caller is cancelled or times out.
    pub async fn shutdown(self) -> IoResult<()> {
        let owner = tokio::spawn(self.shutdown_owned());
        tokio::time::timeout(SHUTDOWN_TIMEOUT, owner)
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::TimedOut,
                    "in-process runtime shutdown remains owned and pending",
                )
            })?
            .map_err(|error| IoError::other(format!("in-process shutdown owner failed: {error}")))?
    }

    async fn shutdown_owned(mut self) -> IoResult<()> {
        let observation_id = self.observation_id;
        codex_protocol::timing::client_probe::record(101, observation_id, 0, 0, 0, [0; 4]);
        // Explicit shutdown abandons unread events. Close the receiver first so
        // a runtime task blocked on a must-deliver event can observe closure,
        // finish cleanup, and avoid leaving both transport directions stalled.
        self.event_rx.close();
        let mut runtime_handle = self.runtime_handle;
        let (done_tx, done_rx) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;

        // The public observer is bounded, but the retained owner must never
        // cancel Shutdown admission: sender clones can outlive that deadline.
        if self.client.client_tx.send(InProcessClientMessage::Shutdown { done_tx }).await.is_ok() {
            let _ = done_rx.await;
        }

        // A completed join can contain a panic; timeout success is not task success.
        let runtime_result =
            finish_in_process_task("runtime", &mut runtime_handle, None, deadline).await;
        let result = match runtime_result {
            // Closing unread lossless delivery can finish the runtime before it
            // consumes Shutdown. Its joined result is the authoritative barrier,
            // including persistence, even if the queued acknowledgement was lost.
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) | Err(error) => Err(error),
        };
        codex_protocol::timing::client_probe::record(102, observation_id, 0, 0, 0, [u64::from(result.is_ok()), 0, 0, 0]);
        result
    }

    pub fn sender(&self) -> InProcessClientSender {
        self.client.clone()
    }

    /// Returns the successful initialize result after [`start`] completes.
    pub fn initialize_response(&self) -> Option<&codex_app_server_protocol_types::Result> {
        self.initialize_response.as_ref()
    }
}

/// Starts an in-process app-server runtime and performs initialize handshake.
///
/// This function sends `initialize` followed by `initialized` before returning
/// the handle, so callers receive a ready-to-use runtime. If initialize fails,
/// the runtime is shut down and an `InvalidData` error is returned.
pub async fn start(mut args: InProcessStartArgs) -> IoResult<InProcessClientHandle> {
    if let Ok(Some(err)) = check_execpolicy_for_warnings(&args.config.config_layer_stack).await {
        args.config_warnings.push(crate::exec_policy_config_warning(&err));
    }
    let initialize = args.initialize.clone();
    let mut client = start_uninitialized(args).await?;
    let sender = client.sender();

    let initialize_response = sender
        .request(ClientRequest::Initialize {
            request_id: RequestId::Integer(0),
            params: initialize,
        })
        .await?;
    let initialize_response = match initialize_response {
        Ok(response) => response,
        Err(error) => {
            let _ = client.shutdown().await;
            return Err(IoError::new(
                ErrorKind::InvalidData,
                format!("in-process initialize failed: {}", error.message),
            ));
        }
    };
    client.initialize_response = Some(initialize_response);
    sender.notify(ClientNotification::Initialized)?;
    codex_protocol::timing::client_probe::record(100, client.observation_id, 0, 0, 0, [0; 4]);

    Ok(client)
}

async fn start_uninitialized(args: InProcessStartArgs) -> IoResult<InProcessClientHandle> {
    let channel_capacity = args.channel_capacity.max(1);
    let installation_id = resolve_installation_id(&args.config.codex_home).await?;
    let (client_tx, mut client_rx) = mpsc::channel::<InProcessClientMessage>(
        channel_capacity.saturating_add(CONTROL_REQUEST_CAPACITY),
    );
    let (event_tx, event_rx) = mpsc::channel::<InProcessServerEvent>(channel_capacity);

    let runtime_handle = tokio::spawn(async move {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<OutgoingEnvelope>(channel_capacity);
        let (notification_tx, mut notification_rx) =
            mpsc::channel::<OutgoingEnvelope>(channel_capacity);
        let auth_manager =
            AuthManager::shared_from_config(args.config.as_ref(), args.enable_codex_api_key_env)
                .await;
        let analytics_events_client =
            analytics_events_client_from_config(Arc::clone(&auth_manager), args.config.as_ref());
        let outgoing_message_sender = Arc::new(
            OutgoingMessageSender::new(outgoing_tx, analytics_events_client.clone())
                .with_notification_sender(notification_tx),
        );

        let (writer_tx, mut writer_rx) = mpsc::channel::<QueuedOutgoingMessage>(channel_capacity);
        let outbound_initialized = Arc::new(AtomicBool::new(false));
        let delivery_failure = tokio_util::sync::CancellationToken::new();
        let outbound_experimental_api_enabled = Arc::new(AtomicBool::new(false));
        outgoing_message_sender
            .connection_opened_with_runtime(
                IN_PROCESS_CONNECTION_ID,
                codex_app_server_transport::ConnectionOrigin::InProcess,
                Arc::clone(&outbound_initialized),
                delivery_failure.clone(),
            )
            .await;
        let outbound_opted_out_notification_methods = Arc::new(
            crate::transport::OutboundNotificationOptOuts::new(HashSet::new()),
        );

        // Transcript backpressure belongs to this single ordered owner. Responses,
        // interrupts and client replies continue through the independent runtime loop.
        let mut notification_handle = tokio::spawn({
            let event_tx = event_tx.clone();
            let delivery_failure = delivery_failure.clone();
            let initialized = Arc::clone(&outbound_initialized);
            let experimental = Arc::clone(&outbound_experimental_api_enabled);
            let opt_outs = Arc::clone(&outbound_opted_out_notification_methods);
            async move {
                let _delivery_owner = delivery_failure.clone().drop_guard();
                let (tx, mut rx) = mpsc::channel(1);
                let mut connections = HashMap::from([(
                    IN_PROCESS_CONNECTION_ID,
                    OutboundConnectionState::new(
                        tx,
                        initialized,
                        experimental,
                        opt_outs,
                        // This owner drains the single-slot writer immediately below.
                        // Do not select deferred physical-stdio delivery: it can enqueue
                        // after try_recv, stranding the last event without another wakeup.
                        Some(delivery_failure.clone()),
                        delivery_failure.clone(),
                    ).with_in_process_delivery(),
                )]);
                let mut delivery_tasks = tokio::task::JoinSet::new();
                let mut skipped = 0;
                loop {
                    let envelope = tokio::select! {
                        _ = delivery_failure.cancelled() => break,
                        envelope = notification_rx.recv() => match envelope { Some(envelope) => envelope, None => break },
                    };
                    route_outgoing_envelope(&mut connections, envelope, &mut delivery_tasks).await;
                    // Filtering can intentionally omit a notification.
                    let Ok(queued) = rx.try_recv() else { continue };
                    let delivery = if let OutgoingMessage::AppServerNotification(notification) = queued.message {
                        tokio::select! {
                            _ = delivery_failure.cancelled() => NotificationDelivery::Closed,
                            delivered = forward_server_notification(&event_tx, &mut skipped, notification) => delivered,
                        }
                    } else {
                        NotificationDelivery::Delivered
                    };
                    if delivery == NotificationDelivery::Closed {
                        delivery_failure.cancel();
                        break;
                    }
                    if delivery == NotificationDelivery::Delivered
                        && let Some(ack) = queued.write_complete_tx
                    {
                        let _ = ack.send(std::time::Instant::now());
                    }
                }
            }
        });

        let mut outbound_connections = HashMap::<ConnectionId, OutboundConnectionState>::new();
        outbound_connections.insert(
            IN_PROCESS_CONNECTION_ID,
            OutboundConnectionState::new(
                writer_tx,
                Arc::clone(&outbound_initialized),
                Arc::clone(&outbound_experimental_api_enabled),
                Arc::clone(&outbound_opted_out_notification_methods),
                Some(delivery_failure.clone()),
                delivery_failure.clone(),
            ).with_in_process_delivery(),
        );
        let outbound_shutdown = delivery_failure.clone();
        let mut outbound_handle = tokio::spawn(async move {
            let _worker_owner = outbound_shutdown.clone().drop_guard();
            let mut delivery_tasks = tokio::task::JoinSet::new();
            loop {
                let envelope = tokio::select! {
                    _ = outbound_shutdown.cancelled() => break,
                    envelope = outgoing_rx.recv() => match envelope {
                        Some(envelope) => envelope,
                        None => break,
                    },
                };
                route_outgoing_envelope(&mut outbound_connections, envelope, &mut delivery_tasks).await;
            }
        });

        let processor_outgoing = Arc::clone(&outgoing_message_sender);
        let config_manager = ConfigManager::new(
            args.config.codex_home.to_path_buf(),
            args.cli_overrides,
            args.loader_overrides,
            args.strict_config,
            args.cloud_config_bundle,
            args.arg0_paths.clone(),
            args.thread_config_loader,
        );
        let (processor_tx, mut processor_rx) = mpsc::channel::<ProcessorCommand>(
            channel_capacity.saturating_add(CONTROL_REQUEST_CAPACITY),
        );
        let processor_shutdown = tokio_util::sync::CancellationToken::new();
        let stop_processor = processor_shutdown.clone();
        let processor_lifetime = delivery_failure.clone().drop_guard();
        let mut processor_handle = tokio::spawn(async move {
            let _worker_owner = processor_lifetime;
            let processor = Arc::new(MessageProcessor::new(MessageProcessorArgs {
                outgoing: Arc::clone(&processor_outgoing),
                analytics_events_client,
                arg0_paths: args.arg0_paths,
                config: args.config,
                config_manager,
                environment_manager: args.environment_manager,
                feedback: args.feedback,
                log_db: args.log_db,
                state_db: args.state_db,
                config_warnings: args.config_warnings,
                session_source: args.session_source,
                auth_manager,
                installation_id,
                rpc_transport: AppServerRpcTransport::InProcess,
                remote_control_handle: None,
                plugin_startup_tasks: crate::PluginStartupTasks::Start,
            }));
            let mut thread_created_rx = processor.subscribe_thread_created();
            let session = Arc::new(ConnectionSessionState::new());
            let mut listen_for_threads = true;
            let mut thread_attachment_tasks = tokio::task::JoinSet::new();

            loop {
                tokio::select! {
                    biased;
                    _ = stop_processor.cancelled() => break,
                    command = processor_rx.recv() => {
                        match command {
                            Some(ProcessorCommand::Request { request, trace }) => {
                                let was_initialized = session.initialized();
                                processor
                                    .process_client_request(
                                        IN_PROCESS_CONNECTION_ID,
                                        *request,
                                        Arc::clone(&session),
                                        &outbound_initialized,
                                        trace,
                                    )
                                    .await;
                                let is_initialized = session.initialized();
                                if !was_initialized && is_initialized {
                                    let opted_out_notification_methods_snapshot =
                                        session.opted_out_notification_methods();
                                    let experimental_api_enabled =
                                        session.experimental_api_enabled();

                                    if !outbound_opted_out_notification_methods
                                        .replace(opted_out_notification_methods_snapshot)
                                    {
                                        warn!("failed to update outbound opted-out notifications");
                                    }
                                    outbound_experimental_api_enabled.store(
                                        experimental_api_enabled,
                                        Ordering::Release,
                                    );

                                    processor.send_initialize_notifications()
                                        .await;
                                }
                            }
                            Some(ProcessorCommand::Notification(notification)) => {
                                processor.process_client_notification(notification).await;
                            }
                            None => {
                                break;
                            }
                        }
                    }
                    result = thread_attachment_tasks.join_next(), if !thread_attachment_tasks.is_empty() => {
                        match result {
                            Some(Ok(keep_listening)) => listen_for_threads = keep_listening,
                            Some(Err(error)) => warn!(%error, "thread listener attachment worker failed"),
                            None => {}
                        }
                    }
                    created = thread_created_rx.recv(), if listen_for_threads && thread_attachment_tasks.is_empty() => {
                        let connection_ids = if session.initialized() {
                            vec![IN_PROCESS_CONNECTION_ID]
                        } else {
                            Vec::<ConnectionId>::new()
                        };
                        let processor = Arc::clone(&processor);
                        thread_attachment_tasks.spawn(async move {
                            processor.handle_thread_created_event(created, connection_ids).await
                        });
                    }
                }
            }

            processor.clear_runtime_references();
            thread_attachment_tasks.shutdown().await;
            processor.cancel_active_login().await;
            processor
                .connection_closed(IN_PROCESS_CONNECTION_ID, &session)
                .await;
            processor.clear_all_listeners()
                .await;
            processor.drain_background_tasks().await;
            let result = processor.shutdown_threads_with_result()
                .await;
            processor.wait_for_background_tasks().await;
            processor.finish_shutdown_delivery().await;
            result
        });
        let mut pending_request_responses =
            HashMap::<RequestId, oneshot::Sender<PendingClientRequestResponse>>::new();
        let mut pending_controls = HashSet::new();
        let mut shutdown_ack = None;
        let mut skipped_events = 0usize;
        let mut notification_result = None;
        let mut outbound_result = None;
        let mut processor_result = None;

        loop {
            if delivery_failure.is_cancelled() {
                break;
            }
            tokio::select! {
                _ = delivery_failure.cancelled() => break,
                result = &mut notification_handle => { notification_result = Some(result); break; }
                result = &mut outbound_handle => { outbound_result = Some(result); break; }
                result = &mut processor_handle => { processor_result = Some(result); break; }
                message = client_rx.recv() => {
                    match message {
                        Some(InProcessClientMessage::Request { request, trace, response_tx }) => {
                            let request = *request;
                            let request_id = request.id().clone();
                            let control = in_process_request_is_control(&request);
                            let at_capacity = if control {
                                pending_controls.len() >= CONTROL_REQUEST_CAPACITY
                            } else {
                                pending_request_responses.len().saturating_sub(pending_controls.len()) >= channel_capacity
                            };
                            match pending_request_responses.entry(request_id.clone()) {
                                Entry::Vacant(entry) => {
                                    // Cancellation of a waiter does not cancel dispatched work;
                                    // retain its ID and capacity until the real reply arrives.
                                    if at_capacity {
                                        let _ = response_tx.send(Err(overloaded_error(
                                            OverloadReason::InFlightTaskCapacity,
                                            "in-process app-server in-flight request capacity reached",
                                        )));
                                        continue;
                                    }
                                    entry.insert(response_tx);
                                    if control { pending_controls.insert(request_id.clone()); }
                                }
                                Entry::Occupied(_) => {
                                    let _ = response_tx.send(Err(invalid_request(format!(
                                        "duplicate request id: {request_id:?}"
                                    ))));
                                    continue;
                                }
                            }

                            match codex_app_server_transport::try_send_with_control_reserve(
                                &processor_tx, ProcessorCommand::Request { request: Box::new(request), trace },
                                control, CONTROL_REQUEST_CAPACITY,
                            ) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    pending_controls.remove(&request_id);
                                    if let Some(response_tx) =
                                        pending_request_responses.remove(&request_id)
                                    {
                                        let _ = response_tx.send(Err(overloaded_error(
                                            OverloadReason::InProcessRequestQueue,
                                            "in-process app-server request queue is full",
                                        )));
                                    }
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    pending_controls.remove(&request_id);
                                    if let Some(response_tx) =
                                        pending_request_responses.remove(&request_id)
                                    {
                                        let _ = response_tx.send(Err(internal_error(
                                            "in-process app-server request processor is closed",
                                        )));
                                    }
                                    break;
                                }
                            }
                        }
                        Some(InProcessClientMessage::Notification { notification }) => {
                            match codex_app_server_transport::try_send_with_control_reserve(
                                &processor_tx, ProcessorCommand::Notification(notification), false, CONTROL_REQUEST_CAPACITY,
                            ) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    warn!("dropping in-process client notification (queue full)");
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    break;
                                }
                            }
                        }
                        Some(InProcessClientMessage::ServerRequestResponse { request_id, result }) => {
                            outgoing_message_sender
                                .notify_client_response(IN_PROCESS_CONNECTION_ID, request_id, result)
                                .await;
                        }
                        Some(InProcessClientMessage::ServerRequestError { request_id, error }) => {
                            outgoing_message_sender
                                .notify_client_error(IN_PROCESS_CONNECTION_ID, request_id, error)
                                .await;
                        }
                        Some(InProcessClientMessage::Shutdown { done_tx }) => {
                            shutdown_ack = Some(done_tx);
                            break;
                        }
                        None => {
                            break;
                        }
                    }
                }
                queued_message = writer_rx.recv() => {
                    let Some(queued_message) = queued_message else {
                        break;
                    };
                    let outgoing_message = queued_message.message;
                    let mut delivered = true;
                    match outgoing_message {
                        OutgoingMessage::Response(response) => {
                            pending_controls.remove(&response.id);
                            if let Some(response_tx) = pending_request_responses.remove(&response.id) {
                                let _ = response_tx.send(Ok(response.result));
                            } else {
                                warn!(
                                    request_id = ?response.id,
                                    "dropping unmatched in-process response"
                                );
                            }
                        }
                        OutgoingMessage::Error(error) => {
                            pending_controls.remove(&error.id);
                            if let Some(response_tx) = pending_request_responses.remove(&error.id) {
                                let _ = response_tx.send(Err(error.error));
                            } else {
                                warn!(
                                    request_id = ?error.id,
                                    "dropping unmatched in-process error response"
                                );
                            }
                        }
                        OutgoingMessage::Request(request) => {
                            if let Err(delivery_error) =
                                forward_server_request(&event_tx, &mut skipped_events, request)
                            {
                                let (error, request) = match delivery_error {
                                    ServerRequestDeliveryError::Full(request) => (
                                        overloaded_error(
                                            OverloadReason::InProcessServerRequestQueue,
                                            "in-process server request queue is full",
                                        ),
                                        request,
                                    ),
                                    ServerRequestDeliveryError::Closed(request) => (
                                        internal_error(
                                            "in-process server request consumer is closed",
                                        ),
                                        request,
                                    ),
                                };
                                let request_id = request.id().clone();
                                outgoing_message_sender
                                    .notify_client_error(
                                        IN_PROCESS_CONNECTION_ID,
                                        request_id,
                                        error,
                                    )
                                    .await;
                            }
                        }
                        OutgoingMessage::AppServerNotification(notification) => {
                            let delivery = tokio::select! {
                                _ = delivery_failure.cancelled() => NotificationDelivery::Closed,
                                delivered = forward_server_notification(
                                    &event_tx,
                                    &mut skipped_events,
                                    notification,
                                ) => delivered,
                            };
                            if delivery == NotificationDelivery::Closed {
                                break;
                            }
                            delivered = delivery == NotificationDelivery::Delivered;
                        }
                    }
                    if delivered && let Some(write_complete_tx) = queued_message.write_complete_tx {
                        let _ = write_complete_tx.send(std::time::Instant::now());
                    }
                }
            }
        }

        // Stop not-yet-started commands before settling their callers. An
        // already-entered handler keeps its owner through committed cleanup.
        processor_shutdown.cancel();
        delivery_failure.cancel();
        let cleanup_deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;
        let notification_result = finish_in_process_task(
            "notification",
            &mut notification_handle,
            notification_result,
            cleanup_deadline,
        )
        .await;
        // Settle requests queued while lossless event delivery was blocked.
        drop(client_rx);
        drop(writer_rx);
        drop(processor_tx);
        outgoing_message_sender
            .cancel_all_requests(Some(internal_error(
                "in-process app-server runtime is shutting down",
            )))
            .await;
        // Drop the runtime's last sender before awaiting the router task so
        // `outgoing_rx.recv()` can observe channel closure and exit cleanly.
        drop(outgoing_message_sender);
        for (_, response_tx) in pending_request_responses {
            let _ = response_tx.send(Err(internal_error(
                "in-process app-server runtime is shutting down",
            )));
        }

        let processor_result = finish_in_process_task(
            "processor",
            &mut processor_handle,
            processor_result,
            cleanup_deadline,
        )
        .await
        .and_then(|result| result);
        let outbound_result = finish_in_process_task(
            "outbound",
            &mut outbound_handle,
            outbound_result,
            cleanup_deadline,
        )
        .await;
        for (owner, result) in [
            ("notification", &notification_result),
            ("processor", &processor_result),
            ("outbound", &outbound_result),
        ] {
            if let Err(error) = result {
                warn!(owner, %error, "in-process worker shutdown failed");
            }
        }
        // Cleanup all peers before returning the first failure.
        let shutdown_result = notification_result
            .and(processor_result)
            .and(outbound_result);

        if let Some(done_tx) = shutdown_ack {
            let _ = done_tx.send(
                shutdown_result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| IoError::new(error.kind(), error.to_string())),
            );
        }
        shutdown_result
    });


    Ok(InProcessClientHandle {
        observation_id: codex_protocol::timing::client_probe::instance(),
        client: InProcessClientSender { client_tx },
        event_rx,
        runtime_handle,
        initialize_response: None,
    })
}

async fn finish_in_process_task<T>(
    name: &str,
    handle: &mut tokio::task::JoinHandle<T>,
    completed: Option<std::result::Result<T, tokio::task::JoinError>>,
    deadline: tokio::time::Instant,
) -> IoResult<T> {
    let joined = match completed {
        Some(result) => result,
        None => match tokio::time::timeout_at(deadline, &mut *handle).await {
            Ok(result) => result,
            Err(_) => {
                // The enclosing retained shutdown task keeps joining actual
                // descendants. Never abort a processor between commit and its
                // required background/core persistence cleanup.
                warn!(
                    name,
                    "in-process shutdown observation expired; owner continues draining"
                );
                handle.await
            }
        },
    };
    joined.map_err(|error| IoError::other(format!("in-process {name} task failed: {error}")))
}

