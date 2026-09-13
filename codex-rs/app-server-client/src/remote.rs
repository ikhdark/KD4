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
const REMOTE_APP_SERVER_MAX_WEBSOCKET_MESSAGE_SIZE: usize = 128 << 20;
const METHOD_NOT_FOUND_ERROR_CODE: i64 = -32601;
const INVALID_REQUEST_ERROR_CODE: i64 = -32600;
// Tungstenite still needs an HTTP request URI for the WebSocket handshake;
// the bytes travel over the Unix socket, not TCP.
const UDS_WEBSOCKET_HANDSHAKE_URL: &str = "ws://localhost/rpc";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAppServerEndpoint {
    WebSocket {
        websocket_url: String,
        auth_token: Option<String>,
    },
    UnixSocket {
        socket_path: AbsolutePathBuf,
    },
}

impl RemoteAppServerEndpoint {
    /// Whether this endpoint can carry a bearer token without exposing it over
    /// a non-loopback plaintext WebSocket connection.
    pub fn supports_auth_token(&self) -> bool {
        match self {
            Self::WebSocket { websocket_url, .. } => {
                Url::parse(websocket_url).is_ok_and(|url| websocket_url_supports_auth_token(&url))
            }
            Self::UnixSocket { .. } => false,
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
    /// Capacity for both command and event queues (clamped to at least one).
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
                Self::connect_with_stream(channel_capacity, endpoint, stream, initialize_params)
                    .await
            }
            RemoteAppServerEndpoint::UnixSocket { socket_path } => {
                let (endpoint, stream) = connect_unix_socket_endpoint(socket_path).await?;
                Self::connect_with_stream(channel_capacity, endpoint, stream, initialize_params)
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
        )
        .await?;

        let (command_tx, mut command_rx) = mpsc::channel::<RemoteClientCommand>(channel_capacity);
        let (event_tx, event_rx) = mpsc::channel::<AppServerEvent>(channel_capacity);
        let worker_handle = tokio::spawn(async move {
            let mut pending_requests =
                HashMap::<RequestId, oneshot::Sender<IoResult<RequestResult>>>::new();
            let mut pending_delivery = VecDeque::<AppServerEvent>::new();
            let mut skipped_events = 0usize;
            let mut worker_exit_error: Option<(ErrorKind, String)> = None;
            loop {
                tokio::select! {
                    permit = event_tx.reserve(), if !pending_delivery.is_empty() => {
                        let Ok(permit) = permit else {
                            break;
                        };
                        let Some(event) = pending_delivery.pop_front() else {
                            continue;
                        };
                        permit.send(event);
                    }
                    command = command_rx.recv() => {
                        let Some(command) = command else {
                            let _ = stream.close(None).await;
                            break;
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
                                pending_requests.insert(request_id.clone(), response_tx);
                                if let Err(err) = write_jsonrpc_message(
                                    &mut stream,
                                    JSONRPCMessage::Request(*request),
                                    &endpoint,
                                )
                                .await
                                {
                                    let err_message = err.to_string();
                                    let message = format!(
                                        "remote app server at `{endpoint}` write failed: {err_message}"
                                    );
                                    if let Some(response_tx) = pending_requests.remove(&request_id) {
                                        let _ = response_tx.send(Err(err));
                                    }
                                    let _ = deliver_event(
                                        &event_tx,
                                        &mut pending_delivery,
                                        &mut skipped_events,
                                        AppServerEvent::Disconnected {
                                            message: message.clone(),
                                        },
                                    )
                                    .await;
                                    worker_exit_error = Some((ErrorKind::BrokenPipe, message));
                                    break;
                                }
                            }
                            RemoteClientCommand::Notify { notification, response_tx } => {
                                let result = write_jsonrpc_message(
                                    &mut stream,
                                    JSONRPCMessage::Notification(
                                        jsonrpc_notification_from_client_notification(notification),
                                    ),
                                    &endpoint,
                                )
                                .await;
                                let _ = response_tx.send(result);
                            }
                            RemoteClientCommand::ResolveServerRequest {
                                request_id,
                                result,
                                response_tx,
                            } => {
                                let result = write_jsonrpc_message(
                                    &mut stream,
                                    JSONRPCMessage::Response(JSONRPCResponse {
                                        id: request_id,
                                        result,
                                    }),
                                    &endpoint,
                                )
                                .await;
                                let _ = response_tx.send(result);
                            }
                            RemoteClientCommand::RejectServerRequest {
                                request_id,
                                error,
                                response_tx,
                            } => {
                                let result = write_jsonrpc_message(
                                    &mut stream,
                                    JSONRPCMessage::Error(JSONRPCError {
                                        error,
                                        id: request_id,
                                    }),
                                    &endpoint,
                                )
                                .await;
                                let _ = response_tx.send(result);
                            }
                            RemoteClientCommand::Shutdown { response_tx } => {
                                let close_result = stream.close(None).await.or_else(|err| {
                                    if websocket_close_error_is_already_closed(&err) {
                                        Ok(())
                                    } else {
                                        Err(IoError::other(format!(
                                            "failed to close websocket app server `{endpoint}`: {err}"
                                        )))
                                    }
                                });
                                let _ = response_tx.send(close_result);
                                break;
                            }
                        }
                    }
                    message = stream.next(), if pending_delivery.is_empty() => {
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
                                        if let Some(event) = app_server_event_from_notification(notification)
                                            && let Err(err) = forward_remote_event(
                                                &event_tx,
                                                &mut pending_delivery,
                                                &mut skipped_events,
                                                event,
                                            ) {
                                                warn!(%err, "failed to deliver remote app-server event");
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
                                                        if let Err(err) = write_jsonrpc_message(
                                                            &mut stream,
                                                            JSONRPCMessage::Error(JSONRPCError {
                                                                error: overloaded_error(
                                                                    OverloadReason::RemoteAppServerEventQueue,
                                                                    "remote app-server event queue is full",
                                                                ),
                                                                id: request_id,
                                                            }),
                                                            &endpoint,
                                                        )
                                                        .await
                                                        {
                                                            let message = format!(
                                                                "remote app server at `{endpoint}` failed to reject a saturated server request: {err}"
                                                            );
                                                            let _ = deliver_event(
                                                                &event_tx,
                                                                &mut pending_delivery,
                                                                &mut skipped_events,
                                                                AppServerEvent::Disconnected {
                                                                    message: message.clone(),
                                                                },
                                                            )
                                                            .await;
                                                            worker_exit_error =
                                                                Some((ErrorKind::BrokenPipe, message));
                                                            break;
                                                        }
                                                    }
                                                    Ok(None) => {}
                                                    Err(err) => {
                                                        warn!(%err, "failed to deliver remote app-server server request");
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(err) => {
                                                warn!(%err, method, "rejecting unknown remote app-server request");
                                                if let Err(reject_err) = write_jsonrpc_message(
                                                    &mut stream,
                                                    JSONRPCMessage::Error(JSONRPCError {
                                                        error: JSONRPCErrorError {
                                                            code: METHOD_NOT_FOUND_ERROR_CODE,
                                                            message: format!(
                                                                "unsupported remote app-server request `{method}`"
                                                            ),
                                                            data: None,
                                                        },
                                                        id: request_id,
                                                    }),
                                                    &endpoint,
                                                )
                                                .await
                                                {
                                                    let err_message = reject_err.to_string();
                                                    let message = format!(
                                                        "remote app server at `{endpoint}` write failed: {err_message}"
                                                    );
                                                    let _ = deliver_event(
                                                        &event_tx,
                                                        &mut pending_delivery,
                                                        &mut skipped_events,
                                                        AppServerEvent::Disconnected {
                                                            message: message.clone(),
                                                        },
                                                    )
                                                    .await;
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
                                        let _ = deliver_event(
                                            &event_tx,
                                            &mut pending_delivery,
                                            &mut skipped_events,
                                            AppServerEvent::Disconnected {
                                                message: message.clone(),
                                            },
                                        )
                                        .await;
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
                                let _ = deliver_event(
                                    &event_tx,
                                    &mut pending_delivery,
                                    &mut skipped_events,
                                    AppServerEvent::Disconnected {
                                        message: message.clone(),
                                    },
                                )
                                .await;
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
                                let _ = deliver_event(
                                    &event_tx,
                                    &mut pending_delivery,
                                    &mut skipped_events,
                                    AppServerEvent::Disconnected {
                                        message: message.clone(),
                                    },
                                )
                                .await;
                                worker_exit_error = Some((ErrorKind::InvalidData, message));
                                break;
                            }
                            None => {
                                let message = format!(
                                    "remote app server at `{endpoint}` closed the connection"
                                );
                                let _ = deliver_event(
                                    &event_tx,
                                    &mut pending_delivery,
                                    &mut skipped_events,
                                    AppServerEvent::Disconnected {
                                        message: message.clone(),
                                    },
                                )
                                .await;
                                worker_exit_error = Some((ErrorKind::UnexpectedEof, message));
                                break;
                            }
                        }
                    }
                }
            }

            let (err_kind, err_message) = worker_exit_error.unwrap_or_else(|| {
                (
                    ErrorKind::BrokenPipe,
                    "remote app-server worker channel is closed".to_string(),
                )
            });
            for (_, response_tx) in pending_requests {
                let _ = response_tx.send(Err(IoError::new(err_kind, err_message.clone())));
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
        let mut worker_handle = worker_handle;
        drop(event_rx);
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
        let header_value =
            HeaderValue::from_str(&format!("Bearer {auth_token}")).map_err(|err| {
                IoError::new(
                    ErrorKind::InvalidInput,
                    format!("invalid remote authorization header value: {err}"),
                )
            })?;
        request.headers_mut().insert(AUTHORIZATION, header_value);
    }

    let stream = timeout(CONNECT_TIMEOUT, async {
        let connector = tokio::task::spawn_blocking(|| {
            ensure_rustls_crypto_provider();
            maybe_build_rustls_client_config_with_custom_ca()
                .map_err(IoError::from)
                .map(|config| config.map(Connector::Rustls))
        })
        .await
        .map_err(|error| {
            IoError::other(format!("remote TLS configuration task failed: {error}"))
        })??;
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
                            if let Some(event) = app_server_event_from_notification(notification) {
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

        write_jsonrpc_message(
            stream,
            JSONRPCMessage::Notification(jsonrpc_notification_from_client_notification(
                ClientNotification::Initialized,
            )),
            endpoint,
        )
        .await?;

        let initialize_response = initialize_response.ok_or_else(|| {
            IoError::new(ErrorKind::InvalidData, "missing remote initialize response")
        })?;
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

fn app_server_event_from_notification(notification: JSONRPCNotification) -> Option<AppServerEvent> {
    match ServerNotification::try_from(notification) {
        Ok(notification) => Some(AppServerEvent::ServerNotification(notification)),
        Err(_) => None,
    }
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
    pending_delivery: &mut VecDeque<AppServerEvent>,
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
        if *skipped_events > 0 {
            pending_delivery.push_back(AppServerEvent::Lagged {
                skipped: *skipped_events,
            });
            *skipped_events = 0;
        }
        pending_delivery.push_back(event);
        return Ok(None);
    }

    if *skipped_events > 0 {
        match event_tx.try_send(AppServerEvent::Lagged {
            skipped: *skipped_events,
        }) {
            Ok(()) => *skipped_events = 0,
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
    pending_delivery: &mut VecDeque<AppServerEvent>,
    skipped_events: &mut usize,
    event: AppServerEvent,
) -> IoResult<()> {
    let rejected = forward_remote_event(event_tx, pending_delivery, skipped_events, event)?;
    debug_assert!(rejected.is_none());
    while let Some(event) = pending_delivery.pop_front() {
        event_tx.send(event).await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "remote app-server event consumer channel is closed",
            )
        })?;
    }
    Ok(())
}

fn jsonrpc_request_from_client_request(request: ClientRequest) -> IoResult<JSONRPCRequest> {
    JSONRPCRequest::try_from(request).map_err(|err| IoError::new(ErrorKind::InvalidInput, err))
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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_connect_deadline_covers_queued_ca_preparation() {
        use futures::FutureExt;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind remote peer");
        listener.set_nonblocking(true).expect("nonblocking peer");
        let websocket_url = format!("ws://localhost:{}", listener.local_addr().unwrap().port());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .max_blocking_threads(1)
            .build()
            .expect("runtime with one CA worker");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            started_tx.send(()).expect("worker started");
            release_rx.recv().expect("release worker");
        });
        started_rx.recv().expect("blocking worker occupied");

        let result = runtime.block_on(async {
            let mut connecting =
                Box::pin(RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
                    endpoint: RemoteAppServerEndpoint::WebSocket {
                        websocket_url,
                        auth_token: Some("test-token".to_owned()),
                    },
                    client_name: "deadline-test".to_owned(),
                    client_version: "1".to_owned(),
                    experimental_api: true,
                    mcp_server_openai_form_elicitation: false,
                    opt_out_notification_methods: Vec::new(),
                    channel_capacity: 1,
                }));
            // The normal entry queues its actual CA preparation behind the
            // occupied blocking worker. Its connection timer must still run.
            assert!(connecting.as_mut().now_or_never().is_none());
            tokio::time::advance(CONNECT_TIMEOUT).await;
            connecting.as_mut().now_or_never()
        });
        // Release and drain blocking work before asserting, including when
        // the old implementation has failed to produce a timeout result.
        release_tx.send(()).expect("release blocking worker");
        runtime.block_on(async {
            blocker.await.expect("blocking worker finished");
            tokio::task::spawn_blocking(|| {})
                .await
                .expect("CA preparation drained");
        });
        let error = result
            .expect("connection deadline includes waiting for CA preparation")
            .err()
            .expect("queued CA preparation must time out");
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert!(
            error
                .to_string()
                .contains("timed out connecting to remote app server")
        );
        assert_eq!(
            listener
                .accept()
                .expect_err("no connection after expiry")
                .kind(),
            ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn remote_startup_deadline_covers_blocked_initialize_write() {
        use tokio::io::AsyncReadExt;
        use tokio_tungstenite::tungstenite::protocol::Role;

        let (transport, mut peer) = tokio::io::duplex(8);
        let stream = WebSocketStream::from_raw_socket(transport, Role::Client, None).await;
        let result = timeout(
            INITIALIZE_TIMEOUT + Duration::from_secs(2),
            RemoteAppServerClient::connect_with_stream(
                1,
                "blocked-peer".to_owned(),
                stream,
                crate::initialize_params("test", "1", false, false, &[]),
            ),
        )
        .await
        .expect("the connection deadline must include writing initialize");
        let error = result.err().expect("an unread peer must time out");
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        let mut partial_request = Vec::new();
        timeout(
            Duration::from_secs(1),
            peer.read_to_end(&mut partial_request),
        )
        .await
        .expect("failed initialization must close its transport")
        .expect("read partial request before EOF");
        assert!(
            !partial_request.is_empty(),
            "the real initialize write must start"
        );
    }

    #[tokio::test]
    async fn remote_shutdown_deadline_covers_full_queue_and_reaps_worker() {
        use tokio::io::AsyncReadExt;
        use tokio_tungstenite::tungstenite::protocol::Role;

        let (transport, peer) = tokio::io::duplex(1024);
        let stream = WebSocketStream::from_raw_socket(transport, Role::Client, None).await;
        let mut peer = WebSocketStream::from_raw_socket(peer, Role::Server, None).await;
        let peer_initialize = async {
            let message = peer
                .next()
                .await
                .expect("initialize frame")
                .expect("valid frame");
            let JSONRPCMessage::Request(request) =
                serde_json::from_str(&message.into_text().expect("text frame")).expect("request")
            else {
                panic!("expected initialize request");
            };
            assert_eq!(request.method, "initialize");
            peer.send(Message::Text(
                serde_json::to_string(&JSONRPCMessage::Response(JSONRPCResponse {
                    id: request.id,
                    result: serde_json::json!({ "userAgent": "test/1" }),
                }))
                .expect("response JSON")
                .into(),
            ))
            .await
            .expect("initialize response");
            let initialized = peer
                .next()
                .await
                .expect("initialized frame")
                .expect("valid frame");
            let JSONRPCMessage::Notification(notification) =
                serde_json::from_str(&initialized.into_text().expect("text frame"))
                    .expect("initialized notification")
            else {
                panic!("expected initialized notification");
            };
            assert_eq!(notification.method, "initialized");
            peer
        };
        let (client, mut peer) = tokio::join!(
            RemoteAppServerClient::connect_with_stream(
                1,
                "blocked-peer".to_owned(),
                stream,
                crate::initialize_params("test", "1", false, false, &[]),
            ),
            peer_initialize,
        );
        let client = client.expect("initialize real remote worker");
        let worker = client.worker_handle.abort_handle();
        let first_handle = client.request_handle();
        let first = tokio::spawn(async move {
            first_handle
                .request_json_rpc(JSONRPCRequest {
                    id: RequestId::Integer(1),
                    method: "test/blocked".to_owned(),
                    params: Some(serde_json::json!({ "payload": "x".repeat(4096) })),
                    trace: None,
                })
                .await
        });
        // Seeing the frame begin proves the normal worker consumed the first
        // command and is blocked writing its remaining bytes to the full pipe.
        peer.get_mut()
            .read_exact(&mut [0_u8; 1])
            .await
            .expect("request write begins");
        let second_handle = client.request_handle();
        let second = tokio::spawn(async move {
            second_handle
                .request_json_rpc(JSONRPCRequest {
                    id: RequestId::Integer(2),
                    method: "test/queued".to_owned(),
                    params: None,
                    trace: None,
                })
                .await
        });
        timeout(Duration::from_secs(1), async {
            while client.command_tx.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second request fills the command queue");

        let shutdown = timeout(SHUTDOWN_TIMEOUT + Duration::from_secs(2), client.shutdown()).await;
        if shutdown.is_err() {
            worker.abort();
        }
        shutdown
            .expect("shutdown deadline must include queue admission")
            .expect("shutdown");
        assert!(
            worker.is_finished(),
            "shutdown must reap its blocked worker"
        );
        for request in [first, second] {
            let error = request
                .await
                .expect("request task")
                .expect_err("pending request must fail");
            assert_eq!(error.kind(), ErrorKind::BrokenPipe);
        }
    }

    #[test]
    fn cancelled_remote_request_is_abandoned_before_dispatch() {
        let (response_tx, response_rx) = oneshot::channel();
        drop(response_rx);
        let command = RemoteClientCommand::Request {
            request: Box::new(
                jsonrpc_request_from_client_request(ClientRequest::GetAccount {
                    request_id: RequestId::Integer(1),
                    params: codex_app_server_protocol::GetAccountParams {
                        refresh_token: false,
                    },
                })
                .expect("account request encodes"),
            ),
            response_tx,
        };

        assert!(command.is_abandoned_request());
    }

    #[test]
    fn confirmed_performance_client_adapters_preserve_jsonrpc_shape() {
        let request = jsonrpc_request_from_client_request(ClientRequest::GetAccount {
            request_id: RequestId::Integer(7),
            params: codex_app_server_protocol::GetAccountParams {
                refresh_token: true,
            },
        })
        .expect("account request encodes");
        assert_eq!(request.method, "account/read");
        assert_eq!(request.id, RequestId::Integer(7));
        assert_eq!(
            request.params,
            Some(serde_json::json!({ "refreshToken": true }))
        );

        assert_eq!(
            jsonrpc_notification_from_client_notification(ClientNotification::Initialized),
            JSONRPCNotification {
                method: "initialized".to_string(),
                params: None,
            }
        );
    }

    #[tokio::test]
    async fn shutdown_tolerates_worker_exit_after_command_is_queued() {
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let (_event_tx, event_rx) = mpsc::channel::<AppServerEvent>(1);
        let worker_handle = tokio::spawn(async move {
            let _ = command_rx.recv().await;
        });
        let client = RemoteAppServerClient {
            command_tx,
            event_rx,
            pending_events: VecDeque::new(),
            initialize_response: AppServerInitializeResponse::from_json(serde_json::json!({})),
            worker_handle,
        };

        client
            .shutdown()
            .await
            .expect("shutdown should complete when worker exits first");
    }
}
