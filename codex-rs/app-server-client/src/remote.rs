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
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::OverloadReason;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result as JsonRpcResult;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::overloaded_error;
use codex_app_server_protocol::server_notification_requires_delivery;
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
const REMOTE_APP_SERVER_MAX_WEBSOCKET_MESSAGE_SIZE: usize = 128 << 20;
const METHOD_NOT_FOUND_ERROR_CODE: i64 = -32601;
const INVALID_REQUEST_ERROR_CODE: i64 = -32600;
const MAX_PENDING_REMOTE_EVENTS: usize = 1024;
const MAX_PENDING_REMOTE_EVENT_BYTES: usize = 16 << 20;

#[derive(Default)]
struct PendingRemoteEvents {
    events: VecDeque<(AppServerEvent, usize)>,
    bytes: usize,
}

impl PendingRemoteEvents {
    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }


    fn pop_front(&mut self) -> Option<AppServerEvent> {
        let (event, bytes) = self.events.pop_front()?;
        self.bytes -= bytes;
        Some(event)
    }

    fn push_back(&mut self, event: AppServerEvent) -> IoResult<()> {
        fn exhausted() -> IoError {
            IoError::new(ErrorKind::OutOfMemory,
                "remote app-server required event retention budget exceeded; connection retired")
        }
        let remaining = MAX_PENDING_REMOTE_EVENT_BYTES.saturating_sub(self.bytes);
        let overhead = std::mem::size_of::<AppServerEvent>();
        if self.events.len() >= MAX_PENDING_REMOTE_EVENTS || remaining < overhead {
            return Err(exhausted());
        }
        // Count serialized content without allocating a second payload. Charge
        // conservatively for decoded strings/containers as well as the enum.
        struct Counter { bytes: usize, limit: usize }
        impl std::io::Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> IoResult<usize> {
                self.bytes = self.bytes.checked_add(bytes.len())
                    .filter(|next| *next <= self.limit).ok_or_else(exhausted)?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> IoResult<()> {
                Ok(())
            }
        }
        let mut count = Counter { bytes: 0, limit: (remaining - overhead) / 8 };
        match &event {
            AppServerEvent::ServerNotification(value) => serde_json::to_writer(&mut count, value),
            AppServerEvent::ServerRequest(value) => serde_json::to_writer(&mut count, value),
            AppServerEvent::Disconnected { message } => serde_json::to_writer(&mut count, message),
            AppServerEvent::Lagged { skipped } => serde_json::to_writer(&mut count, skipped),
        }
        .map_err(|error| if error.io_error_kind() == Some(ErrorKind::OutOfMemory) {
            exhausted()
        } else { IoError::other(error) })?;
        let bytes = count
            .bytes
            .saturating_mul(8)
            .saturating_add(std::mem::size_of::<AppServerEvent>());
        if self.events.len() >= MAX_PENDING_REMOTE_EVENTS
            || bytes > MAX_PENDING_REMOTE_EVENT_BYTES.saturating_sub(self.bytes)
        {
            return Err(IoError::new(
                ErrorKind::OutOfMemory,
                "remote app-server required event retention budget exceeded; connection retired",
            ));
        }
        self.bytes += bytes;
        self.events.push_back((event, bytes));
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
                .field("websocket_url", websocket_url)
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

impl RemoteAppServerEndpoint {
    /// Whether this endpoint can carry a bearer token without exposing it over
    /// a non-loopback plaintext WebSocket connection.
    pub fn supports_auth_token(&self) -> bool {
        match self {
            Self::WebSocket { websocket_url, .. } => {
                Url::parse(websocket_url).is_ok_and(|url| websocket_url_supports_auth_token(&url))
            }
            Self::UnixSocket { .. } | Self::DiscoveredUnixSocket { .. } => false,
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
    // None is an ordered close, after all previously admitted frames.
    message: Option<JSONRPCMessage>,
    response_tx: Option<oneshot::Sender<IoResult<()>>>,
}

fn queue_remote_write(
    writer_tx: &mpsc::Sender<RemoteWrite>,
    message: Option<JSONRPCMessage>,
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
    event_rx: mpsc::Receiver<AppServerEvent>,
    pending_events: VecDeque<AppServerEvent>,
    initialize_response: AppServerInitializeResponse,
    worker_handle: tokio::task::JoinHandle<()>,
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
        let (event_tx, event_rx) = mpsc::channel::<AppServerEvent>(channel_capacity);
        let worker_handle = tokio::spawn(async move {
            let (mut sink, mut stream) = stream.split();
            let (writer_tx, mut writer_rx) = mpsc::channel::<RemoteWrite>(channel_capacity);
            let writer_endpoint = endpoint.clone();
            let mut writer_handle = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
                while let Some(write) = writer_rx.recv().await {
                    let close = write.message.is_none();
                    let result = timeout(WRITE_TIMEOUT, async {
                        if let Some(message) = write.message {
                            let payload = serde_json::to_string(&message).map_err(IoError::other)?;
                            sink.send(Message::Text(payload.into())).await.map_err(|error| {
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
            let mut pending_requests =
                HashMap::<RequestId, oneshot::Sender<IoResult<RequestResult>>>::new();
            let mut pending_delivery = PendingRemoteEvents::default();
            let mut skipped_events = 0usize;
            let mut worker_exit_error: Option<(ErrorKind, String)> = None;
            loop {
                tokio::select! {
                    _ = event_tx.closed() => break,
                    result = &mut writer_handle => {
                        writer_finished = true;
                        match result {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => worker_exit_error = Some((error.kind(), error.to_string())),
                            Err(error) => worker_exit_error = Some((ErrorKind::BrokenPipe, format!("remote app-server writer failed: {error}"))),
                        }
                        break;
                    }
                    permit = event_tx.reserve(), if !pending_delivery.is_empty() => {
                        let Ok(permit) = permit else {
                            break;
                        };
                        let Some(event) = pending_delivery.pop_front() else {
                            continue;
                        };
                        permit.send(event);
                    }
                    // Wait for writer ownership only as a select branch, never
                    // inside command processing where it would suspend reads.
                    _ = writer_tx.reserve(), if !closing && writer_tx.capacity() == 0 => {}
                    command = command_rx.recv(), if !closing && writer_tx.capacity() > 0 => {
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
                    message = stream.next(), if pending_delivery.is_empty() || pending_requests.values().any(|waiter| !waiter.is_closed()) => {
                        match message {
                            Some(Ok(Message::Text(text))) => {
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
                                                warn!(%err, method, "rejecting unknown remote app-server request");
                                                if let Err(reject_err) = queue_remote_write(
                                                    &writer_tx,
                                                    Some(JSONRPCMessage::Error(JSONRPCError {
                                                        error: JSONRPCErrorError {
                                                            code: METHOD_NOT_FOUND_ERROR_CODE,
                                                            message: format!(
                                                                "unsupported remote app-server request `{method}`"
                                                            ),
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
        });

        Ok(Self {
            command_tx,
            event_rx,
            pending_events: pending_events.into(),
            initialize_response,
            worker_handle,
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
            return Some(event);
        }
        self.event_rx.recv().await
    }

    pub async fn shutdown(self) -> IoResult<()> {
        let Self {
            command_tx,
            event_rx,
            pending_events: _pending_events,
            initialize_response: _initialize_response,
            worker_handle,
        } = self;
        let mut worker_handle = tokio_util::task::AbortOnDropHandle::new(worker_handle);
        let (response_tx, response_rx) = oneshot::channel();
        // Queue admission can block behind a stalled socket write, so it must
        // share the same deadline as the close acknowledgement and worker exit.
        let shutdown_result = timeout(SHUTDOWN_TIMEOUT, async {
            let close_result = if command_tx
                .send(RemoteClientCommand::Shutdown { response_tx })
                .await
                .is_ok()
            {
                match response_rx.await {
                    Ok(result) => result,
                    Err(_) => Ok(()),
                }
            } else {
                Ok(())
            };
            // Consumer loss aborts the writer, so retain this receiver until
            // the ordered WebSocket close has been acknowledged.
            drop(event_rx);
            let _ = (&mut worker_handle).await;
            close_result
        })
        .await;
        match shutdown_result {
            Ok(result) => result,
            Err(_elapsed) => {
                worker_handle.abort();
                let _ = worker_handle.await;
                Ok(())
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

async fn connect_websocket_endpoint(
    websocket_url: String,
    auth_token: Option<String>,
) -> IoResult<(String, WebSocketStream<MaybeTlsStream<TcpStream>>)> {
    let url = Url::parse(&websocket_url).map_err(|err| {
        IoError::new(
            ErrorKind::InvalidInput,
            format!("invalid websocket URL `{websocket_url}`: {err}"),
        )
    })?;
    if auth_token.is_some() && !websocket_url_supports_auth_token(&url) {
        return Err(IoError::new(
            ErrorKind::InvalidInput,
            format!(
                "remote auth tokens require `wss://` or loopback `ws://` URLs; got `{websocket_url}`"
            ),
        ));
    }

    let mut request = url.as_str().into_client_request().map_err(|err| {
        IoError::new(
            ErrorKind::InvalidInput,
            format!("invalid websocket URL `{websocket_url}`: {err}"),
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
                "failed to connect to remote app server at `{websocket_url}`: {err}"
            ))
        })
    })
    .await
    .map_err(|_| {
        IoError::new(
            ErrorKind::TimedOut,
            format!("timed out connecting to remote app server at `{websocket_url}`"),
        )
    })??;

    Ok((websocket_url, stream))
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

async fn initialize_remote_connection<S>(
    stream: &mut WebSocketStream<S>,
    endpoint: &str,
    params: InitializeParams,
    pending_event_capacity: usize,
    initialize_timeout: Duration,
    expected_home: Option<&AbsolutePathBuf>,
) -> IoResult<(Vec<AppServerEvent>, AppServerInitializeResponse)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    timeout(initialize_timeout, async {
        let initialize_request_id = RequestId::String("initialize".to_string());
        let mut pending_events = Vec::new();
        let mut initialize_response = None;
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

        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    let message = serde_json::from_str::<JSONRPCMessage>(&text).map_err(|err| {
                        IoError::other(format!(
                            "remote app server at `{endpoint}` sent invalid initialize response: {err}"
                        ))
                    })?;
                    match message {
                        JSONRPCMessage::Response(response) if response.id == initialize_request_id => {
                            initialize_response =
                                Some(AppServerInitializeResponse::from_json(response.result));
                            break Ok(());
                        }
                        JSONRPCMessage::Error(error) if error.id == initialize_request_id => {
                            break Err(IoError::other(format!(
                                "remote app server at `{endpoint}` rejected initialize: {}",
                                error.error.message
                            )));
                        }
                        JSONRPCMessage::Notification(notification) => {
                            if let Some(event) = app_server_event_from_notification(notification)? {
                                if pending_events.len() >= pending_event_capacity {
                                    break Err(IoError::new(
                                        ErrorKind::InvalidData,
                                        format!(
                                            "remote app server at `{endpoint}` exceeded the pending initialize event capacity of {pending_event_capacity}"
                                        ),
                                    ));
                                }
                                pending_events.push(event);
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
                                        METHOD_NOT_FOUND_ERROR_CODE,
                                        format!("unsupported remote app-server request `{method}`"),
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

        let initialize_response = initialize_response.ok_or_else(|| {
            IoError::new(ErrorKind::InvalidData, "missing remote initialize response")
        })?;
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
    event_tx: &mpsc::Sender<AppServerEvent>,
    pending_delivery: &mut PendingRemoteEvents,
    skipped_events: &mut usize,
    event: AppServerEvent,
) -> IoResult<Option<ServerRequest>> {
    if event_tx.is_closed() {
        return Err(IoError::new(
            ErrorKind::BrokenPipe,
            "remote app-server event consumer channel is closed",
        ));
    }

    if remote_event_requires_delivery(&event) {
        // Do not interpose a worker select pass (or a spurious backlog) while
        // the consumer has room. Retained events and lag markers keep priority.
        let event = if pending_delivery.is_empty() && *skipped_events == 0 {
            match event_tx.try_send(event) {
                Ok(()) => return Ok(None),
                Err(mpsc::error::TrySendError::Full(event)) => event,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(IoError::new(
                        ErrorKind::BrokenPipe,
                        "remote app-server event consumer channel is closed",
                    ));
                }
            }
        } else {
            event
        };
        if *skipped_events > 0 {
            pending_delivery.push_back(AppServerEvent::Lagged {
                skipped: *skipped_events,
            })?;
            *skipped_events = 0;
        }
        pending_delivery.push_back(event)?;
        return Ok(None);
    }

    // A best-effort event must not overtake retained lossless events.
    if !pending_delivery.is_empty() {
        *skipped_events = skipped_events.saturating_add(1);
        return Ok(match event {
            AppServerEvent::ServerRequest(request) => Some(request),
            _ => None,
        });
    }

    let mut sent_lagged = false;
    if *skipped_events > 0 {
        match event_tx.try_send(AppServerEvent::Lagged {
            skipped: *skipped_events,
        }) {
            Ok(()) => {
                *skipped_events = 0;
                sent_lagged = true;
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                *skipped_events = skipped_events.saturating_add(1);
                return Ok(match event {
                    AppServerEvent::ServerRequest(request) => Some(request),
                    _ => None,
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(IoError::new(
                    ErrorKind::BrokenPipe,
                    "remote app-server event consumer channel is closed",
                ));
            }
        }
    }

    match event_tx.try_send(event) {
        Ok(()) => Ok(None),
        Err(mpsc::error::TrySendError::Full(event)) => {
            if sent_lagged && matches!(&event, AppServerEvent::ServerRequest(_)) {
                pending_delivery.push_back(event)?;
                return Ok(None);
            }
            *skipped_events = skipped_events.saturating_add(1);
            Ok(match event {
                AppServerEvent::ServerRequest(request) => Some(request),
                _ => None,
            })
        }
        Err(mpsc::error::TrySendError::Closed(_)) => Err(IoError::new(
            ErrorKind::BrokenPipe,
            "remote app-server event consumer channel is closed",
        )),
    }
}

async fn deliver_event(
    event_tx: &mpsc::Sender<AppServerEvent>,
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
            .send(AppServerEvent::Lagged {
                skipped: *skipped_events,
            })
            .await
            .map_err(|_| IoError::new(ErrorKind::BrokenPipe, "event consumer closed"))?;
        *skipped_events = 0;
    }
    event_tx
        .send(event)
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
