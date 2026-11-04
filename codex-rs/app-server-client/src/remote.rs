/*
This module implements the remote app-server client transport.

It owns the remote connection lifecycle, including the initialize/initialized
handshake, JSON-RPC request/response routing, server-request resolution, and
notification streaming. Remote connections always carry WebSocket frames, over
either TCP WebSocket URLs or local Unix sockets. The rest of the crate uses the
same `AppServerEvent` surface for both in-process and remote transports, so
callers such as the TUI can switch between them without changing their
higher-level session logic.
*/

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::time::Duration;

use crate::AppServerEvent;
use crate::AppServerInitializeResponse;
use crate::RequestResult;
use crate::SHUTDOWN_TIMEOUT;
use crate::TypedRequestError;
use codex_app_server_protocol_types::ClientNotification;
use codex_app_server_protocol_types::ClientRequest;
use codex_app_server_protocol_types::InitializeParams;
use codex_app_server_protocol_types::JSONRPCError;
use codex_app_server_protocol_types::JSONRPCErrorError;
use codex_app_server_protocol_types::JSONRPCMessage;
use codex_app_server_protocol_types::JSONRPCNotification;
use codex_app_server_protocol_types::JSONRPCRequest;
use codex_app_server_protocol_types::JSONRPCResponse;
use codex_app_server_protocol_types::OverloadReason;
use codex_app_server_protocol_types::RequestId;
use codex_app_server_protocol_types::Result as JsonRpcResult;
use codex_app_server_protocol_types::ServerNotification;
use codex_app_server_protocol_types::ServerRequest;
use codex_app_server_protocol_types::overloaded_error;
use codex_app_server_protocol_types::server_notification_requires_delivery;
use codex_http_client::maybe_build_rustls_client_config_with_custom_ca;
use codex_uds::UnixStream;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;
use futures::SinkExt;
use futures::StreamExt;
use serde::de::DeserializeOwned;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_tungstenite::Connector;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::client_async_with_config;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::warn;
use url::Url;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const HEARTBEAT_PONG_TIMEOUT: Duration = Duration::from_secs(30);
const REMOTE_APP_SERVER_MAX_WEBSOCKET_MESSAGE_SIZE: usize = 128 << 20;
const METHOD_NOT_FOUND_ERROR_CODE: i64 = -32601;
const INVALID_REQUEST_ERROR_CODE: i64 = -32600;
const INVALID_PARAMS_ERROR_CODE: i64 = -32602;
const MAX_PENDING_REMOTE_EVENTS: usize = 1024;
const MAX_PENDING_REMOTE_EVENT_BYTES: usize = 16 << 20;

// Keep one maximum supported frame charge in addition to the normal backlog.
// The multiplier is the existing conservative decoded-size accounting policy.
const MAX_REMOTE_EVENT_OWNERSHIP_BYTES: usize =
    MAX_PENDING_REMOTE_EVENT_BYTES + REMOTE_APP_SERVER_MAX_WEBSOCKET_MESSAGE_SIZE * 8 + std::mem::size_of::<AppServerEvent>();

struct BudgetedRemoteEvent {
    event: AppServerEvent,
    _credit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl BudgetedRemoteEvent {
    fn metadata(event: AppServerEvent) -> Self { Self { event, _credit: None } }
}

struct PendingRemoteEvents {
    events: VecDeque<BudgetedRemoteEvent>,
    budget: std::sync::Arc<tokio::sync::Semaphore>,
}

impl Default for PendingRemoteEvents {
    fn default() -> Self {
        Self { events: VecDeque::new(), budget: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_REMOTE_EVENT_OWNERSHIP_BYTES)) }
    }
}

impl PendingRemoteEvents {
    fn is_empty(&self) -> bool { self.events.is_empty() }
    fn pop_front(&mut self) -> Option<BudgetedRemoteEvent> { self.events.pop_front() }

    fn acquire_wire(&self, bytes: usize) -> IoResult<tokio::sync::OwnedSemaphorePermit> {
        let charge = bytes.saturating_mul(8).saturating_add(std::mem::size_of::<AppServerEvent>());
        let charge = u32::try_from(charge).map_err(IoError::other)?;
        self.budget.clone().try_acquire_many_owned(charge).map_err(|_| IoError::new(
            ErrorKind::OutOfMemory, "remote app-server aggregate decoded/queued/retained event byte budget exceeded; connection retired",
        ))
    }

    fn push_back(&mut self, event: BudgetedRemoteEvent) -> IoResult<()> {
        if self.events.len() >= MAX_PENDING_REMOTE_EVENTS {
            return Err(IoError::new(ErrorKind::OutOfMemory, "remote app-server event retention count exceeded; connection retired"));
        }
        self.events.push_back(event);
        Ok(())
    }
}
// Tungstenite still needs an HTTP request URI for the WebSocket handshake;
// the bytes travel over the Unix socket, not TCP.
const UDS_WEBSOCKET_HANDSHAKE_URL: &str = "ws://localhost/rpc";

#[derive(Clone, PartialEq, Eq)]
pub enum RemoteAppServerEndpoint {
    WebSocket {
        websocket_url: String,
        auth_token: Option<String>,
    },
    UnixSocket {
        socket_path: AbsolutePathBuf,
    },
    /// Implicit local discovery must validate the advertised runtime home.
    DiscoveredUnixSocket {
        socket_path: AbsolutePathBuf,
        expected_home: AbsolutePathBuf,
    },
}

impl std::fmt::Debug for RemoteAppServerEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WebSocket {
                websocket_url,
                auth_token,
            } => f
                .debug_struct("WebSocket")
                .field("websocket_url", &redacted_websocket_endpoint(websocket_url))
                .field("auth_token", &auth_token.as_ref().map(|_| "[REDACTED]"))
                .finish(),
            Self::UnixSocket { socket_path } => f
                .debug_struct("UnixSocket")
                .field("socket_path", socket_path)
                .finish(),
            Self::DiscoveredUnixSocket {
                socket_path,
                expected_home,
            } => f
                .debug_struct("DiscoveredUnixSocket")
                .field("socket_path", socket_path)
                .field("expected_home", expected_home)
                .finish(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteAppServerConnectArgs {
    pub endpoint: RemoteAppServerEndpoint,
    pub client_name: String,
    pub client_version: String,
    pub experimental_api: bool,
    pub mcp_server_openai_form_elicitation: bool,
    pub opt_out_notification_methods: Vec<String>,
    /// Capacity for command/event queues and dispatched requests awaiting replies
    /// (clamped to at least one). Excess requests receive a retryable overload error.
    pub channel_capacity: usize,
}
impl RemoteAppServerConnectArgs {
    pub(crate) fn initialize_params(&self) -> InitializeParams {
        crate::initialize_params(
            &self.client_name,
            &self.client_version,
            self.experimental_api,
            self.mcp_server_openai_form_elicitation,
            &self.opt_out_notification_methods,
        )
    }
}

pub(crate) fn websocket_url_supports_auth_token(url: &Url) -> bool {
    match (url.scheme(), url.host()) {
        ("wss", Some(_)) => true,
        ("ws", Some(url::Host::Domain(domain))) => domain.eq_ignore_ascii_case("localhost"),
        ("ws", Some(url::Host::Ipv4(addr))) => addr.is_loopback(),
        ("ws", Some(url::Host::Ipv6(addr))) => addr.is_loopback(),
        _ => false,
    }
}

enum RemoteClientCommand {
    Request {
        request: Box<JSONRPCRequest>,
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

struct RemoteWrite {
    message: RemoteWriteMessage,
    response_tx: Option<oneshot::Sender<IoResult<()>>>,
}

enum RemoteWriteMessage {
    JsonRpc(JSONRPCMessage),
    Ping(Vec<u8>),
    // An ordered close, after all previously admitted frames.
    Close,
}

fn queue_remote_write(
    writer_tx: &mpsc::Sender<RemoteWrite>,
    message: Option<JSONRPCMessage>,
    response_tx: Option<oneshot::Sender<IoResult<()>>>,
) -> IoResult<()> {
    queue_remote_frame(
        writer_tx,
        message.map_or(RemoteWriteMessage::Close, RemoteWriteMessage::JsonRpc),
        response_tx,
    )
}

fn queue_remote_frame(
    writer_tx: &mpsc::Sender<RemoteWrite>,
    message: RemoteWriteMessage,
    response_tx: Option<oneshot::Sender<IoResult<()>>>,
) -> IoResult<()> {
    writer_tx
        .try_send(RemoteWrite { message, response_tx })
        .map_err(|error| {
            // Never await writer-queue capacity in the read coordinator. If
            // bounded ordered ownership is exhausted, retire the connection.
            let message = "remote app-server writer queue unavailable; connection retired";
            if let Some(response_tx) = error.into_inner().response_tx {
                let _ = response_tx.send(Err(IoError::new(ErrorKind::BrokenPipe, message)));
            }
            IoError::new(ErrorKind::BrokenPipe, message)
        })
}

impl RemoteClientCommand {
    /// Drop requests cancelled while waiting in the bounded command channel so
    /// stale work cannot delay newer requests or reach the remote app-server.
    fn is_abandoned_request(&self) -> bool {
        matches!(
            self,
            Self::Request { response_tx, .. } if response_tx.is_closed()
        )
    }
}

pub struct RemoteAppServerClient {
    command_tx: mpsc::Sender<RemoteClientCommand>,
    event_rx: mpsc::Receiver<BudgetedRemoteEvent>,
    pending_events: PendingRemoteEvents,
    initialize_response: AppServerInitializeResponse,
    worker_handle: tokio::task::JoinHandle<IoResult<()>>,
    force_shutdown: tokio_util::sync::CancellationToken,
}

#[derive(Clone)]
pub struct RemoteAppServerRequestHandle {
    command_tx: mpsc::Sender<RemoteClientCommand>,
}

impl RemoteAppServerClient {
    pub async fn connect(args: RemoteAppServerConnectArgs) -> IoResult<Self> {
        let channel_capacity = args.channel_capacity.max(1);
        let initialize_params = args.initialize_params();
        match args.endpoint {
            RemoteAppServerEndpoint::WebSocket {
                websocket_url,
                auth_token,
            } => {
                let (endpoint, stream) =
                    connect_websocket_endpoint(websocket_url, auth_token).await?;
                Self::connect_with_stream(
                    channel_capacity,
                    endpoint,
                    stream,
                    initialize_params,
                    None,
                )
                .await
            }
            RemoteAppServerEndpoint::UnixSocket { socket_path } => {
                let (endpoint, stream) = connect_unix_socket_endpoint(socket_path).await?;
                Self::connect_with_stream(
                    channel_capacity,
                    endpoint,
                    stream,
                    initialize_params,
                    None,
                )
                .await
            }
            RemoteAppServerEndpoint::DiscoveredUnixSocket {
                socket_path,
                expected_home,
            } => {
                let (endpoint, stream) = connect_unix_socket_endpoint(socket_path).await?;
                Self::connect_with_stream(
                    channel_capacity,
                    endpoint,
                    stream,
                    initialize_params,
                    Some(expected_home),
                )
                .await
            }
        }
    }

    pub fn server_version(&self) -> Option<&str> {
        self.initialize_response.server_version()
    }

    pub fn codex_home(&self) -> Option<&str> {
        self.initialize_response.codex_home()
    }

    pub fn initialize_response(&self) -> &AppServerInitializeResponse {
        &self.initialize_response
    }

    async fn connect_with_stream<S>(
        channel_capacity: usize,
        endpoint: String,
        stream: WebSocketStream<S>,
        initialize_params: InitializeParams,
        expected_home: Option<AbsolutePathBuf>,
    ) -> IoResult<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut stream = stream;
        let (pending_events, initialize_response) = initialize_remote_connection(
            &mut stream,
            &endpoint,
            initialize_params,
            channel_capacity,
            INITIALIZE_TIMEOUT,
            expected_home.as_ref(),
        )
        .await?;

        let (command_tx, mut command_rx) = mpsc::channel::<RemoteClientCommand>(channel_capacity);
        let (event_tx, event_rx) = mpsc::channel::<BudgetedRemoteEvent>(channel_capacity);
        let event_budget = pending_events.budget.clone();
        let force_shutdown = tokio_util::sync::CancellationToken::new();
        let worker_shutdown = force_shutdown.clone();
        let worker_handle = tokio::spawn(async move {
            let (mut sink, mut stream) = stream.split();
            let (writer_tx, mut writer_rx) = mpsc::channel::<RemoteWrite>(channel_capacity);
            let writer_endpoint = endpoint.clone();
            let mut writer_handle = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
                while let Some(write) = writer_rx.recv().await {
                    let close = matches!(&write.message, RemoteWriteMessage::Close);
                    let result = timeout(WRITE_TIMEOUT, async {
                        let frame = match write.message {
                            RemoteWriteMessage::JsonRpc(message) => Some(Message::Text(
                                serde_json::to_string(&message).map_err(IoError::other)?.into(),
                            )),
                            RemoteWriteMessage::Ping(payload) => Some(Message::Ping(payload.into())),
                            RemoteWriteMessage::Close => None,
                        };
                        if let Some(frame) = frame {
                            sink.send(frame).await.map_err(|error| {
                                IoError::other(format!(
                                    "failed to write websocket message to `{writer_endpoint}`: {error}"
                                ))
                            })
                        } else {
                            sink.close().await.or_else(|error| {
                                if websocket_close_error_is_already_closed(&error) {
                                    Ok(())
                                } else {
                                    Err(IoError::other(format!(
                                        "failed to close websocket app server `{writer_endpoint}`: {error}"
                                    )))
                                }
                            })
                        }
                    }).await.unwrap_or_else(|_| Err(IoError::new(
                        ErrorKind::TimedOut,
                        format!("remote app-server write to `{writer_endpoint}` timed out; connection retired"),
                    )));
                    if let Some(response_tx) = write.response_tx {
                        let _ = response_tx.send(result.as_ref().map(|_| ()).map_err(|error| {
                            IoError::new(error.kind(), error.to_string())
                        }));
                    }
                    // A canceled/partial write is never followed by another frame.
                    result?;
                    if close { break; }
                }
                IoResult::Ok(())
            }));
            let mut writer_finished = false;
            let mut closing = false;
            let mut staged_command = None;
            let mut pending_requests =
                HashMap::<RequestId, oneshot::Sender<IoResult<RequestResult>>>::new();
            let mut pending_delivery = PendingRemoteEvents { events: VecDeque::new(), budget: event_budget };
            let mut skipped_events = 0usize;
            let mut worker_exit_error: Option<(ErrorKind, String)> = None;
            let mut heartbeat_deadline = tokio::time::Instant::now() + HEARTBEAT_INTERVAL;
            let mut heartbeat_sequence = 0_u64;
            let mut pending_ping: Option<Vec<u8>> = None;
            let mut pending_ping_write: Option<oneshot::Receiver<IoResult<()>>> = None;
            loop {
                let can_read = pending_delivery.is_empty()
                    || pending_requests.values().any(|waiter| !waiter.is_closed());
                if !can_read || closing {
                    // Local event backpressure is not transport failure. A pong
                    // cannot be observed while this coordinator pauses reads.
                    pending_ping = None;
                    pending_ping_write = None;
                    heartbeat_deadline = tokio::time::Instant::now() + HEARTBEAT_INTERVAL;
                }
                tokio::select! {
                    _ = event_tx.closed() => break,
                    _ = worker_shutdown.cancelled() => {
                        worker_exit_error = Some((ErrorKind::TimedOut, "remote app-server shutdown forced after graceful close deadline".into()));
                        break;
                    }
                    _ = tokio::time::sleep_until(heartbeat_deadline),
                        if can_read && !closing && pending_ping_write.is_none() => {
                        if pending_ping.is_some() {
                            worker_exit_error = Some((ErrorKind::TimedOut, format!(
                                "remote app server at `{endpoint}` did not acknowledge its transport heartbeat; connection retired"
                            )));
                            break;
                        }
                        // Do not treat a busy local writer queue as a dead peer.
                        if writer_tx.capacity() == 0 {
                            heartbeat_deadline = tokio::time::Instant::now() + HEARTBEAT_INTERVAL;
                            continue;
                        }
                        heartbeat_sequence = heartbeat_sequence.wrapping_add(1);
                        let payload = heartbeat_sequence.to_be_bytes().to_vec();
                        let (response_tx, response_rx) = oneshot::channel();
                        if let Err(error) = queue_remote_frame(
                            &writer_tx,
                            RemoteWriteMessage::Ping(payload.clone()),
                            Some(response_tx),
                        ) {
                            worker_exit_error = Some((error.kind(), error.to_string()));
                            break;
                        }
                        pending_ping = Some(payload);
                        pending_ping_write = Some(response_rx);
                    }
                    receipt = async {
                        match pending_ping_write.as_mut() {
                            Some(receipt) => receipt.await,
                            None => std::future::pending().await,
                        }
                    }, if pending_ping_write.is_some() => {
                        pending_ping_write = None;
                        match receipt {
                            Ok(Ok(())) => {
                                // Queueing is not a write receipt. Bound only the
                                // remote acknowledgement wait after actual send.
                                heartbeat_deadline = tokio::time::Instant::now() + HEARTBEAT_PONG_TIMEOUT;
                            }
                            result => {
                                let error = match result {
                                    Ok(Err(error)) => error,
                                    _ => IoError::new(ErrorKind::BrokenPipe, "remote heartbeat writer closed"),
                                };
                                worker_exit_error = Some((error.kind(), error.to_string()));
                                break;
                            }
                        }
                    }
                    result = &mut writer_handle => {
                        writer_finished = true;
                        match result {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => worker_exit_error = Some((error.kind(), error.to_string())),
                            Err(error) => worker_exit_error = Some((ErrorKind::BrokenPipe, format!("remote app-server writer failed: {error}"))),
                        }
                        break;
                    }
                    permit = event_tx.reserve(), if !pending_delivery.is_empty() || skipped_events > 0 => {
                        let Ok(permit) = permit else { break; };
                        if let Some(event) = pending_delivery.pop_front() {
                            permit.send(event);
                        } else {
                            permit.send(BudgetedRemoteEvent::metadata(AppServerEvent::Lagged { skipped: std::mem::take(&mut skipped_events) }));
                        }
                    }
                    // Wait for writer ownership only as a select branch, never
                    // inside command processing where it would suspend reads.
                    _ = writer_tx.reserve(), if !closing && writer_tx.capacity() == 0 => {}
                    command = async {
                        if let Some(command) = staged_command.take() { Some(command) } else { command_rx.recv().await }
                    }, if !closing && (staged_command.is_none() || writer_tx.capacity() > 0) => {
                        let Some(command) = command else {
                            closing = true;
                            if let Err(error) = queue_remote_write(&writer_tx, None, None) {
                                worker_exit_error = Some((error.kind(), error.to_string()));
                                break;
                            }
                            continue;
                        };
                        if command.is_abandoned_request() {
                            continue;
                        }
                        if writer_tx.capacity() == 0 {
                            // Dequeue one bounded command so local duplicate/overload
                            // rejection is independent of unrelated socket congestion.
                            match command {
                                RemoteClientCommand::Request { request, response_tx } if pending_requests.contains_key(&request.id) => {
                                    let _ = response_tx.send(Err(IoError::new(ErrorKind::InvalidInput, format!("duplicate remote app-server request id `{}`", request.id))));
                                }
                                RemoteClientCommand::Request { response_tx, .. } if pending_requests.len() >= channel_capacity => {
                                    let _ = response_tx.send(Ok(Err(overloaded_error(OverloadReason::InFlightTaskCapacity, "remote app-server in-flight request capacity reached"))));
                                }
                                command => staged_command = Some(command),
                            }
                            continue;
                        }
                        match command {
                            RemoteClientCommand::Request { request, response_tx } => {
                                let request_id = request.id.clone();
                                if pending_requests.contains_key(&request_id) {
                                    let _ = response_tx.send(Err(IoError::new(
                                        ErrorKind::InvalidInput,
                                        format!("duplicate remote app-server request id `{request_id}`"),
                                    )));
                                    continue;
                                }
                                // Keep dispatched IDs reserved even if their caller is cancelled:
                                // a late reply must not be routed to a newer request with that ID.
                                if pending_requests.len() >= channel_capacity {
                                    let _ = response_tx.send(Ok(Err(overloaded_error(
                                        OverloadReason::InFlightTaskCapacity,
                                        "remote app-server in-flight request capacity reached",
                                    ))));
                                    continue;
                                }
                                pending_requests.insert(request_id.clone(), response_tx);
                                if let Err(err) = queue_remote_write(
                                    &writer_tx,
                                    Some(JSONRPCMessage::Request(*request)),
                                    None,
                                )
                                {
                                    let err_message = err.to_string();
                                    let message = format!(
                                        "remote app server at `{endpoint}` write failed: {err_message}"
                                    );
                                    if let Some(response_tx) = pending_requests.remove(&request_id) {
                                        let _ = response_tx.send(Err(err));
                                    }
                                    worker_exit_error = Some((ErrorKind::BrokenPipe, message));
                                    break;
                                }
                            }
                            RemoteClientCommand::Notify { notification, response_tx } => {
                                let result = queue_remote_write(
                                    &writer_tx,
                                    Some(JSONRPCMessage::Notification(
                                        jsonrpc_notification_from_client_notification(notification),
                                    )),
                                    Some(response_tx),
                                );
                                if let Err(err) = &result {
                                    worker_exit_error = Some((err.kind(), err.to_string()));
                                }
                                if worker_exit_error.is_some() {
                                    break;
                                }
                            }
                            RemoteClientCommand::ResolveServerRequest {
                                request_id,
                                result,
                                response_tx,
                            } => {
                                let result = queue_remote_write(
                                    &writer_tx,
                                    Some(JSONRPCMessage::Response(JSONRPCResponse {
                                        id: request_id,
                                        result,
                                    })),
                                    Some(response_tx),
                                );
                                if let Err(err) = &result {
                                    worker_exit_error = Some((err.kind(), err.to_string()));
                                }
                                if worker_exit_error.is_some() {
                                    break;
                                }
                            }
                            RemoteClientCommand::RejectServerRequest {
                                request_id,
                                error,
                                response_tx,
                            } => {
                                let result = queue_remote_write(
                                    &writer_tx,
                                    Some(JSONRPCMessage::Error(JSONRPCError {
                                        error,
                                        id: request_id,
                                    })),
                                    Some(response_tx),
                                );
                                if let Err(err) = &result {
                                    worker_exit_error = Some((err.kind(), err.to_string()));
                                }
                                if worker_exit_error.is_some() {
                                    break;
                                }
                            }
                            RemoteClientCommand::Shutdown { response_tx } => {
                                closing = true;
                                if let Err(error) = queue_remote_write(&writer_tx, None, Some(response_tx)) {
                                    worker_exit_error = Some((error.kind(), error.to_string()));
                                    break;
                                }
                            }
                        }
                    }
                    // Retained lossless events pause reads for backpressure, except while a
                    // caller awaits an RPC: its response shares this socket, and a consumer
                    // blocked on that response cannot drain the event queue.
                    message = stream.next(), if can_read => {
                        if let Some(Ok(frame)) = &message {
                            let acknowledged = matches!(frame, Message::Pong(payload)
                                if pending_ping.as_deref() == Some(payload.as_ref()));
                            if acknowledged {
                                pending_ping = None;
                                pending_ping_write = None;
                                heartbeat_deadline = tokio::time::Instant::now() + HEARTBEAT_INTERVAL;
                            }
                        }
                        match message {
                            Some(Ok(Message::Text(text))) => {
                                let wire_credit = match pending_delivery.acquire_wire(text.len()) {
                                    Ok(credit) => credit,
                                    Err(error) => { worker_exit_error = Some((error.kind(), error.to_string())); break; }
                                };
                                match serde_json::from_str::<JSONRPCMessage>(&text) {
                                    Ok(JSONRPCMessage::Response(response)) => {
                                        if let Some(response_tx) = pending_requests.remove(&response.id) {
                                            let _ = response_tx.send(Ok(Ok(response.result)));
                                        }
                                    }
                                    Ok(JSONRPCMessage::Error(error)) => {
                                        if let Some(response_tx) = pending_requests.remove(&error.id) {
                                            let _ = response_tx.send(Ok(Err(error.error)));
                                        }
                                    }
                                    Ok(JSONRPCMessage::Notification(notification)) => {
                                        let event = match app_server_event_from_notification(notification) {
                                            Ok(event) => event,
                                            Err(err) => {
                                                worker_exit_error = Some((err.kind(), err.to_string()));
                                                break;
                                            }
                                        };
                                        if let Some(event) = event
                                            && let Err(err) = forward_remote_event(
                                                &event_tx,
                                                &mut pending_delivery,
                                                &mut skipped_events,
                                                event,
                                                wire_credit,
                                            ) {
                                                warn!(%err, "failed to deliver remote app-server event");
                                                worker_exit_error = Some((err.kind(), err.to_string()));
                                                break;
                                        }
                                    }
                                    Ok(JSONRPCMessage::Request(request)) => {
                                        let request_id = request.id.clone();
                                        let method = request.method.clone();
                                        match ServerRequest::try_from(request) {
                                            Ok(request) => {
                                                match forward_remote_event(
                                                    &event_tx,
                                                    &mut pending_delivery,
                                                    &mut skipped_events,
                                                    AppServerEvent::ServerRequest(request),
                                                    wire_credit,
                                                ) {
                                                    Ok(Some(request)) => {
                                                        let request_id = request.id().clone();
                                                        if let Err(err) = queue_remote_write(
                                                            &writer_tx,
                                                            Some(JSONRPCMessage::Error(JSONRPCError {
                                                                error: overloaded_error(
                                                                    OverloadReason::RemoteAppServerEventQueue,
                                                                    "remote app-server event queue is full",
                                                                ),
                                                                id: request_id,
                                                            })),
                                                            None,
                                                        )
                                                        {
                                                            let message = format!(
                                                                "remote app server at `{endpoint}` failed to reject a saturated server request: {err}"
                                                            );
                                                            worker_exit_error =
                                                                Some((ErrorKind::BrokenPipe, message));
                                                            break;
                                                        }
                                                    }
                                                    Ok(None) => {}
                                                    Err(err) => {
                                                        warn!(%err, "failed to deliver remote app-server server request");
                                                        worker_exit_error = Some((err.kind(), err.to_string()));
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(err) => {
                                                warn!(%err, method, "rejecting unsupported or malformed remote app-server request");
                                                if let Err(reject_err) = queue_remote_write(
                                                    &writer_tx,
                                                    Some(JSONRPCMessage::Error(JSONRPCError {
                                                        error: JSONRPCErrorError {
                                                            code: if ServerRequest::is_known_method(&method) { INVALID_PARAMS_ERROR_CODE } else { METHOD_NOT_FOUND_ERROR_CODE },
                                                            message: if ServerRequest::is_known_method(&method) {
                                                                format!("invalid parameters for remote app-server request `{method}`: {err}")
                                                            } else { format!("unsupported remote app-server request `{method}`") },
                                                            data: None,
                                                        },
                                                        id: request_id,
                                                    })),
                                                    None,
                                                )
                                                {
                                                    let err_message = reject_err.to_string();
                                                    let message = format!(
                                                        "remote app server at `{endpoint}` write failed: {err_message}"
                                                    );
                                                    worker_exit_error =
                                                        Some((ErrorKind::BrokenPipe, message));
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        let message = format!(
                                            "remote app server at `{endpoint}` sent invalid JSON-RPC: {err}"
                                        );
                                        worker_exit_error =
                                            Some((ErrorKind::InvalidData, message));
                                        break;
                                    }
                                }
                            }
                            Some(Ok(Message::Close(frame))) => {
                                let reason = frame
                                    .as_ref()
                                    .map(|frame| frame.reason.to_string())
                                    .filter(|reason| !reason.is_empty())
                                    .unwrap_or_else(|| "connection closed".to_string());
                                let message = format!(
                                    "remote app server at `{endpoint}` disconnected: {reason}"
                                );
                                worker_exit_error = Some((
                                    ErrorKind::ConnectionAborted,
                                    message,
                                ));
                                break;
                            }
                            Some(Ok(Message::Binary(_)))
                            | Some(Ok(Message::Ping(_)))
                            | Some(Ok(Message::Pong(_)))
                            | Some(Ok(Message::Frame(_))) => {}
                            Some(Err(err)) => {
                                let message = format!(
                                    "remote app server at `{endpoint}` transport failed: {err}"
                                );
                                worker_exit_error = Some((ErrorKind::InvalidData, message));
                                break;
                            }
                            None => {
                                let message = format!(
                                    "remote app server at `{endpoint}` closed the connection"
                                );
                                worker_exit_error = Some((ErrorKind::UnexpectedEof, message));
                                break;
                            }
                        }
                    }
                }
            }

            // Retire RPCs and the transport before waiting for the final event's
            // consumer. Rendering a disconnect must never gate request failure.
            command_rx.close();
            drop(command_rx);
            drop(writer_tx);
            if !writer_finished {
                writer_handle.abort();
                let _ = writer_handle.await;
            }
            drop(stream);
            let disconnect_message = worker_exit_error
                .as_ref()
                .map(|(_, message)| message.clone());
            let (err_kind, err_message) = worker_exit_error.unwrap_or_else(|| {
                (
                    ErrorKind::BrokenPipe,
                    "remote app-server worker channel is closed".to_string(),
                )
            });
            let worker_result = disconnect_message.as_ref().map_or(Ok(()), |_| Err(IoError::new(err_kind, err_message.clone())));
            for (_, response_tx) in pending_requests {
                let _ = response_tx.send(Err(IoError::new(err_kind, err_message.clone())));
            }
            if let Some(message) = disconnect_message {
                let _ = deliver_event(
                    &event_tx,
                    &mut pending_delivery,
                    &mut skipped_events,
                    AppServerEvent::Disconnected { message },
                )
                .await;
            }
            worker_result
        });

        Ok(Self {
            command_tx,
            event_rx,
            pending_events,
            initialize_response,
            worker_handle,
            force_shutdown,
        })
    }

    pub fn request_handle(&self) -> RemoteAppServerRequestHandle {
        RemoteAppServerRequestHandle {
            command_tx: self.command_tx.clone(),
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
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(RemoteClientCommand::Notify {
                notification,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "remote app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "remote app-server notify channel is closed",
            )
        })?
    }

    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(RemoteClientCommand::ResolveServerRequest {
                request_id,
                result,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "remote app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "remote app-server resolve channel is closed",
            )
        })?
    }

    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(RemoteClientCommand::RejectServerRequest {
                request_id,
                error,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "remote app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "remote app-server reject channel is closed",
            )
        })?
    }

    pub async fn next_event(&mut self) -> Option<AppServerEvent> {
        if let Some(event) = self.pending_events.pop_front() {
            return Some(event.event);
        }
        self.event_rx.recv().await.map(|event| event.event)
    }

    pub async fn shutdown(self) -> IoResult<()> {
        // Cleanup custody is independent of cancellation of the public waiter.
        tokio::spawn(self.shutdown_owned()).await
            .map_err(|error| IoError::other(format!("remote shutdown owner failed: {error}")))?
    }

    async fn shutdown_owned(self) -> IoResult<()> {
        let Self { command_tx, event_rx, pending_events: _pending_events,
            initialize_response: _initialize_response, mut worker_handle, force_shutdown } = self;
        let mut event_rx = Some(event_rx);
        let (response_tx, response_rx) = oneshot::channel();
        let result = timeout(SHUTDOWN_TIMEOUT, async {
            let close_result = if command_tx.send(RemoteClientCommand::Shutdown { response_tx }).await.is_ok() {
                response_rx.await.unwrap_or_else(|_| Err(IoError::new(ErrorKind::BrokenPipe, "remote close receipt dropped")))
            } else {
                Err(IoError::new(ErrorKind::BrokenPipe, "remote shutdown command channel closed"))
            };
            drop(event_rx.take());
            let joined = (&mut worker_handle).await.map_err(|error| IoError::other(format!("remote coordinator failed: {error}")))?;
            close_result.and(joined)
        }).await;
        match result {
            Ok(result) => result,
            Err(_) => {
                // The coordinator owns abort-and-join of its writer. Never abort
                // that owner and mistake a dropped nested handle for joined cleanup.
                force_shutdown.cancel();
                drop(event_rx.take());
                worker_handle.await.map_err(|error| IoError::other(format!("remote coordinator failed during forced shutdown: {error}")))??;
                Err(IoError::new(ErrorKind::TimedOut, "remote shutdown required forced closure after graceful deadline"))
            }
        }
    }
}

impl RemoteAppServerRequestHandle {
    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        self.request_json_rpc(jsonrpc_request_from_client_request(request)?)
            .await
    }

    pub async fn request_json_rpc(&self, request: JSONRPCRequest) -> IoResult<RequestResult> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(RemoteClientCommand::Request {
                request: Box::new(request),
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "remote app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "remote app-server request channel is closed",
            )
        })?
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        let method = request.method_name().to_string();
        crate::decode_typed_response(method, self.request(request).await)
    }
}

fn redacted_websocket_endpoint(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else { return "<invalid websocket endpoint>".into(); };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

async fn connect_websocket_endpoint(
    websocket_url: String,
    auth_token: Option<String>,
) -> IoResult<(String, WebSocketStream<MaybeTlsStream<TcpStream>>)> {
    let endpoint = redacted_websocket_endpoint(&websocket_url);
    let url = Url::parse(&websocket_url).map_err(|err| {
        IoError::new(
            ErrorKind::InvalidInput,
            format!("invalid websocket URL `{endpoint}`: {err}"),
        )
    })?;
    if auth_token.is_some() && !websocket_url_supports_auth_token(&url) {
        return Err(IoError::new(
            ErrorKind::InvalidInput,
            format!(
                "remote auth tokens require `wss://` or loopback `ws://` URLs; got `{endpoint}`"
            ),
        ));
    }

    let mut request = url.as_str().into_client_request().map_err(|err| {
        IoError::new(
            ErrorKind::InvalidInput,
            format!("invalid websocket URL `{endpoint}`: {err}"),
        )
    })?;
    if let Some(auth_token) = auth_token.as_deref() {
        let mut header_value =
            HeaderValue::from_str(&format!("Bearer {auth_token}")).map_err(|err| {
                IoError::new(
                    ErrorKind::InvalidInput,
                    format!("invalid remote authorization header value: {err}"),
                )
            })?;
        header_value.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, header_value);
    }

    let stream = timeout(CONNECT_TIMEOUT, async {
        let connector = if url.scheme() == "wss" {
            tokio::task::spawn_blocking(|| {
                ensure_rustls_crypto_provider();
                maybe_build_rustls_client_config_with_custom_ca()
                    .map_err(IoError::from)
                    .map(|config| config.map(Connector::Rustls))
            })
            .await
            .map_err(|error| {
                IoError::other(format!("remote TLS configuration task failed: {error}"))
            })??
        } else {
            None
        };
        let websocket_config = remote_websocket_config();
        connect_async_tls_with_config(
            request,
            Some(websocket_config),
            /*disable_nagle*/ false,
            connector,
        )
        .await
        .map(|(stream, _response)| stream)
        .map_err(|err| {
            IoError::other(format!(
                "failed to connect to remote app server at `{endpoint}`: {err}"
            ))
        })
    })
    .await
    .map_err(|_| {
        IoError::new(
            ErrorKind::TimedOut,
            format!("timed out connecting to remote app server at `{endpoint}`"),
        )
    })??;

    Ok((endpoint, stream))
}

async fn connect_unix_socket_endpoint(
    socket_path: AbsolutePathBuf,
) -> IoResult<(String, WebSocketStream<UnixStream>)> {
    let endpoint = format!("unix://{}", socket_path.display());
    let request = UDS_WEBSOCKET_HANDSHAKE_URL
        .into_client_request()
        .map_err(|err| {
            IoError::new(
                ErrorKind::InvalidInput,
                format!("invalid UDS websocket handshake URL: {err}"),
            )
        })?;
    let stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(socket_path.as_path()))
        .await
        .map_err(|_| {
            IoError::new(
                ErrorKind::TimedOut,
                format!("timed out connecting to remote app server at `{endpoint}`"),
            )
        })?
        .map_err(|err| {
            IoError::other(format!(
                "failed to connect to remote app server at `{endpoint}`: {err}"
            ))
        })?;
    let websocket_config = remote_websocket_config();
    let stream = timeout(
        CONNECT_TIMEOUT,
        client_async_with_config(request, stream, Some(websocket_config)),
    )
    .await
    .map_err(|_| {
        IoError::new(
            ErrorKind::TimedOut,
            format!("timed out upgrading remote app server at `{endpoint}`"),
        )
    })?
    .map(|(stream, _response)| stream)
    .map_err(|err| {
        IoError::other(format!(
            "failed to upgrade remote app server at `{endpoint}`: {err}"
        ))
    })?;

    Ok((endpoint, stream))
}

fn remote_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_frame_size(Some(REMOTE_APP_SERVER_MAX_WEBSOCKET_MESSAGE_SIZE))
        .max_message_size(Some(REMOTE_APP_SERVER_MAX_WEBSOCKET_MESSAGE_SIZE))
}

fn validate_initialize_result(raw: &JsonRpcResult) -> IoResult<()> {
    let object = raw.as_object().ok_or_else(|| IoError::new(ErrorKind::InvalidData, "initialize result must be an object"))?;
    for key in ["userAgent", "codexHome"] {
        if !object.get(key).is_some_and(|value| value.as_str().is_some_and(|text| !text.is_empty())) {
            return Err(IoError::new(ErrorKind::InvalidData, format!("initialize result requires nonempty string `{key}`")));
        }
    }
    for key in ["platformFamily", "platformOs"] {
        if object.get(key).is_some_and(|value| !value.is_null() && !value.is_string()) {
            return Err(IoError::new(ErrorKind::InvalidData, format!("initialize result has invalid `{key}`")));
        }
    }
    fn optional<T: DeserializeOwned>(raw: &JsonRpcResult, key: &str) -> IoResult<()> {
        if let Some(value) = raw.get(key).filter(|value| !value.is_null()) {
            T::deserialize(value).map_err(|error| IoError::new(ErrorKind::InvalidData, format!("invalid initialize `{key}`: {error}")))?;
        }
        Ok(())
    }
    optional::<codex_app_server_protocol_types::ServerBuildInfo>(raw, "buildInfo")?;
    optional::<codex_app_server_protocol_types::ServerRuntimeInfo>(raw, "runtimeInfo")?;
    optional::<codex_app_server_protocol_types::ServerCapabilities>(raw, "serverCapabilities")?;
    optional::<codex_app_server_protocol_types::ServerLocalWatermark>(raw, "localWatermark")?;
    Ok(())
}

async fn initialize_remote_connection<S>(
    stream: &mut WebSocketStream<S>,
    endpoint: &str,
    params: InitializeParams,
    pending_event_capacity: usize,
    initialize_timeout: Duration,
    expected_home: Option<&AbsolutePathBuf>,
) -> IoResult<(PendingRemoteEvents, AppServerInitializeResponse)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    timeout(initialize_timeout, async {
        let initialize_request_id = RequestId::String("initialize".to_string());
        let mut pending_events = PendingRemoteEvents::default();
        write_jsonrpc_message(
            stream,
            JSONRPCMessage::Request(jsonrpc_request_from_client_request(
                ClientRequest::Initialize {
                    request_id: initialize_request_id.clone(),
                    params,
                },
            )?),
            endpoint,
        )
        .await?;

        let initialize_response = loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    let message = serde_json::from_str::<JSONRPCMessage>(&text).map_err(|err| {
                        IoError::other(format!(
                            "remote app server at `{endpoint}` sent invalid initialize response: {err}"
                        ))
                    })?;
                    match message {
                        JSONRPCMessage::Response(response) if response.id == initialize_request_id => {
                            validate_initialize_result(&response.result)?;
                            break Ok(AppServerInitializeResponse::from_json(response.result));
                        }
                        JSONRPCMessage::Error(error) if error.id == initialize_request_id => {
                            break Err(IoError::other(format!(
                                "remote app server at `{endpoint}` rejected initialize: {}",
                                error.error.message
                            )));
                        }
                        JSONRPCMessage::Notification(notification) => {
                            if let Some(event) = app_server_event_from_notification(notification)? {
                                if pending_events.events.len() >= pending_event_capacity {
                                    break Err(IoError::new(
                                        ErrorKind::InvalidData,
                                        format!(
                                            "remote app server at `{endpoint}` exceeded the pending initialize event capacity of {pending_event_capacity}"
                                        ),
                                    ));
                                }
                                let credit = pending_events.acquire_wire(text.len())?;
                                pending_events.push_back(BudgetedRemoteEvent { event, _credit: Some(credit) })?;
                            }
                        }
                        JSONRPCMessage::Request(request) => {
                            let request_id = request.id.clone();
                            let method = request.method.clone();
                            let (code, message) = match ServerRequest::try_from(request) {
                                Ok(_) => (
                                    INVALID_REQUEST_ERROR_CODE,
                                    format!(
                                        "remote app-server request `{method}` is not supported before initialization completes"
                                    ),
                                ),
                                Err(err) => {
                                    warn!(%err, method, "rejecting unknown remote app-server request during initialize");
                                    (
                                        if ServerRequest::is_known_method(&method) { INVALID_PARAMS_ERROR_CODE } else { METHOD_NOT_FOUND_ERROR_CODE },
                                        if ServerRequest::is_known_method(&method) { format!("invalid parameters for remote app-server request `{method}`: {err}") }
                                        else { format!("unsupported remote app-server request `{method}`") },
                                    )
                                }
                            };
                            write_jsonrpc_message(
                                stream,
                                JSONRPCMessage::Error(JSONRPCError {
                                    error: JSONRPCErrorError {
                                        code,
                                        message,
                                        data: None,
                                    },
                                    id: request_id,
                                }),
                                endpoint,
                            )
                            .await?;
                        }
                        JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_) => {}
                    }
                }
                Some(Ok(Message::Binary(_)))
                | Some(Ok(Message::Ping(_)))
                | Some(Ok(Message::Pong(_)))
                | Some(Ok(Message::Frame(_))) => {}
                Some(Ok(Message::Close(frame))) => {
                    let reason = frame
                        .as_ref()
                        .map(|frame| frame.reason.to_string())
                        .filter(|reason| !reason.is_empty())
                        .unwrap_or_else(|| "connection closed during initialize".to_string());
                    break Err(IoError::new(
                        ErrorKind::ConnectionAborted,
                        format!(
                            "remote app server at `{endpoint}` closed during initialize: {reason}"
                        ),
                    ));
                }
                Some(Err(err)) => {
                    break Err(IoError::other(format!(
                        "remote app server at `{endpoint}` transport failed during initialize: {err}"
                    )));
                }
                None => {
                    break Err(IoError::new(
                        ErrorKind::UnexpectedEof,
                        format!("remote app server at `{endpoint}` closed during initialize"),
                    ));
                }
            }
        }?;

        if let Some(expected) = expected_home {
            let actual = initialize_response.codex_home()
                .and_then(|home| AbsolutePathBuf::from_absolute_path(home).ok())
                .ok_or_else(|| IoError::new(ErrorKind::InvalidData, format!(
                    "local app-server identity unavailable at `{endpoint}`; expected home `{}`",
                    expected.display()
                )))?;
            if !codex_utils_absolute_path::paths_match_after_normalization(expected, &actual) {
                return Err(IoError::new(ErrorKind::InvalidData, format!(
                    "local app-server home mismatch at `{endpoint}`: expected `{}`, actual `{}`",
                    expected.display(), actual.display()
                )));
            }
        }

        write_jsonrpc_message(
            stream,
            JSONRPCMessage::Notification(jsonrpc_notification_from_client_notification(
                ClientNotification::Initialized,
            )),
            endpoint,
        )
        .await?;

        Ok((pending_events, initialize_response))
    })
    .await
    .map_err(|_| {
        IoError::new(
            ErrorKind::TimedOut,
            format!("timed out initializing remote app server at `{endpoint}`"),
        )
    })?
}

fn app_server_event_from_notification(
    notification: JSONRPCNotification,
) -> IoResult<Option<AppServerEvent>> {
    if !ServerNotification::is_known_method(&notification.method) {
        return Ok(None);
    }
    let method = notification.method.clone();
    ServerNotification::try_from(notification)
        .map(|notification| Some(AppServerEvent::ServerNotification(notification)))
        .map_err(|_| {
            IoError::new(
                ErrorKind::InvalidData,
                format!("invalid payload for known app-server notification `{method}`"),
            )
        })
}

fn remote_event_requires_delivery(event: &AppServerEvent) -> bool {
    match event {
        AppServerEvent::ServerNotification(notification) => {
            server_notification_requires_delivery(notification)
        }
        AppServerEvent::Disconnected { .. } => true,
        AppServerEvent::Lagged { .. } | AppServerEvent::ServerRequest(_) => false,
    }
}

fn forward_remote_event(
    event_tx: &mpsc::Sender<BudgetedRemoteEvent>,
    pending_delivery: &mut PendingRemoteEvents,
    skipped_events: &mut usize,
    event: AppServerEvent,
    credit: tokio::sync::OwnedSemaphorePermit,
) -> IoResult<Option<ServerRequest>> {
    fn closed() -> IoError { IoError::new(ErrorKind::BrokenPipe, "remote app-server event consumer channel is closed") }
    fn rejected(event: BudgetedRemoteEvent) -> Option<ServerRequest> {
        match event.event { AppServerEvent::ServerRequest(request) => Some(request), _ => None }
    }
    if event_tx.is_closed() { return Err(closed()); }
    let required = remote_event_requires_delivery(&event);
    let event = BudgetedRemoteEvent { event, _credit: Some(credit) };
    if required {
        let event = if pending_delivery.is_empty() && *skipped_events == 0 {
            match event_tx.try_send(event) {
                Ok(()) => return Ok(None),
                Err(mpsc::error::TrySendError::Full(event)) => event,
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(closed()),
            }
        } else { event };
        if *skipped_events > 0 {
            pending_delivery.push_back(BudgetedRemoteEvent::metadata(AppServerEvent::Lagged { skipped: std::mem::take(skipped_events) }))?;
        }
        pending_delivery.push_back(event)?;
        return Ok(None);
    }
    if !pending_delivery.is_empty() {
        *skipped_events = skipped_events.saturating_add(1);
        return Ok(rejected(event));
    }
    let mut sent_lagged = false;
    if *skipped_events > 0 {
        match event_tx.try_send(BudgetedRemoteEvent::metadata(AppServerEvent::Lagged { skipped: *skipped_events })) {
            Ok(()) => { *skipped_events = 0; sent_lagged = true; }
            Err(mpsc::error::TrySendError::Full(_)) => {
                *skipped_events = skipped_events.saturating_add(1);
                return Ok(rejected(event));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(closed()),
        }
    }
    match event_tx.try_send(event) {
        Ok(()) => Ok(None),
        Err(mpsc::error::TrySendError::Full(event)) => {
            if sent_lagged && matches!(&event.event, AppServerEvent::ServerRequest(_)) {
                pending_delivery.push_back(event)?;
                return Ok(None);
            }
            *skipped_events = skipped_events.saturating_add(1);
            Ok(rejected(event))
        }
        Err(mpsc::error::TrySendError::Closed(_)) => Err(closed()),
    }
}

async fn deliver_event(
    event_tx: &mpsc::Sender<BudgetedRemoteEvent>,
    pending_delivery: &mut PendingRemoteEvents,
    skipped_events: &mut usize,
    event: AppServerEvent,
) -> IoResult<()> {
    // This terminal path runs only after RPCs and the socket are retired. Drain
    // the bounded queue before sending disconnect, even when it was saturated.
    while let Some(event) = pending_delivery.pop_front() {
        event_tx.send(event).await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "remote app-server event consumer channel is closed",
            )
        })?;
    }
    if *skipped_events > 0 {
        event_tx
            .send(BudgetedRemoteEvent::metadata(AppServerEvent::Lagged {
                skipped: *skipped_events,
            }))
            .await
            .map_err(|_| IoError::new(ErrorKind::BrokenPipe, "event consumer closed"))?;
        *skipped_events = 0;
    }
    let bytes = match &event { AppServerEvent::Disconnected { message } => message.len(), _ => 0 };
    let credit = pending_delivery.acquire_wire(bytes)?;
    event_tx
        .send(BudgetedRemoteEvent { event, _credit: Some(credit) })
        .await
        .map_err(|_| IoError::new(ErrorKind::BrokenPipe, "event consumer closed"))?;
    Ok(())
}

fn jsonrpc_request_from_client_request(request: ClientRequest) -> IoResult<JSONRPCRequest> {
    let mut request = JSONRPCRequest::try_from(request)
        .map_err(|err| IoError::new(ErrorKind::InvalidInput, err))?;
    request.trace = codex_otel::current_span_w3c_trace_context();
    Ok(request)
}

fn jsonrpc_notification_from_client_notification(
    notification: ClientNotification,
) -> JSONRPCNotification {
    match JSONRPCNotification::try_from(notification) {
        Ok(notification) => notification,
        Err(err) => panic!("client notification should encode as JSON-RPC notification: {err}"),
    }
}

async fn write_jsonrpc_message<S>(
    stream: &mut WebSocketStream<S>,
    message: JSONRPCMessage,
    endpoint: &str,
) -> IoResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let payload = serde_json::to_string(&message).map_err(IoError::other)?;
    stream
        .send(Message::Text(payload.into()))
        .await
        .map_err(|err| {
            IoError::other(format!(
                "failed to write websocket message to `{endpoint}`: {err}"
            ))
        })
}

fn websocket_close_error_is_already_closed(err: &TungsteniteError) -> bool {
    match err {
        TungsteniteError::ConnectionClosed | TungsteniteError::AlreadyClosed => true,
        TungsteniteError::Io(err) => matches!(
            err.kind(),
            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::NotConnected
        ),
        _ => false,
    }
}
