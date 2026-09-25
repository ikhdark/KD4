use crate::auth::SharedAuthProvider;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::common::ResponsesWsRequest;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::responses_stream::ResponsesEventError;
use crate::responses_stream::ResponsesEventInterpreter;
use crate::responses_stream::ResponsesStreamMetadata;
use crate::responses_stream::json_headers_to_http_headers;
use crate::telemetry::WebsocketTelemetry;
use bytes::Bytes;
use codex_client::TransportError;
use codex_http_client::HttpClientFactory;
use codex_websocket_client::WebSocketConnection;
use codex_websocket_client::WebSocketConnector;
use futures::SinkExt;
use futures::StreamExt;
use http::HeaderMap;
use http::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use serde_json::map::Map as JsonMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::Utf8Bytes;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tracing::Instrument;
use tracing::Span;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::instrument;
use tungstenite::extensions::ExtensionsConfig;
use tungstenite::extensions::compression::deflate::DeflateConfig;
use tungstenite::protocol::WebSocketConfig;
use url::Url;

struct WsStream {
    tx_command: mpsc::Sender<WsCommand>,
    rx_message: WsIngressReceiver,
    rx_failure: Option<oneshot::Receiver<WsIngressFailure>>,
    pending_failure: Option<WsError>,
    pump_task: tokio::task::JoinHandle<()>,
}

const WEBSOCKET_INGRESS_CAPACITY: usize = 1600;
const WEBSOCKET_INGRESS_MAX_QUEUED_BYTES: usize = 64 * 1024 * 1024;
const WEBSOCKET_INGRESS_OVERFLOW_MESSAGE: &str =
    "responses websocket ingress queue exceeded its bounded capacity";

struct StagedWsMessage {
    message: Message,
    payload_bytes: usize,
}

#[derive(Clone)]
struct WsIngressSender {
    tx: mpsc::Sender<StagedWsMessage>,
    queued_bytes: Arc<AtomicUsize>,
    max_queued_bytes: usize,
}

struct WsIngressReceiver {
    rx: mpsc::Receiver<StagedWsMessage>,
    queued_bytes: Arc<AtomicUsize>,
}

#[derive(Debug, PartialEq, Eq)]
enum WsIngressSendError {
    Full,
    Closed,
}

#[derive(Debug)]
enum WsIngressFailure {
    Transport(WsError),
    Overflow(WsError),
}

fn ws_ingress_channel(
    capacity: usize,
    max_queued_bytes: usize,
) -> (WsIngressSender, WsIngressReceiver) {
    let (tx, rx) = mpsc::channel(capacity);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    (
        WsIngressSender {
            tx,
            queued_bytes: Arc::clone(&queued_bytes),
            max_queued_bytes,
        },
        WsIngressReceiver { rx, queued_bytes },
    )
}

impl WsIngressSender {
    fn try_send(&self, message: Message) -> Result<(), WsIngressSendError> {
        let payload_bytes = message.len();
        if self
            .queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued_bytes| {
                queued_bytes
                    .checked_add(payload_bytes)
                    .filter(|next| *next <= self.max_queued_bytes)
            })
            .is_err()
        {
            return Err(WsIngressSendError::Full);
        }

        let staged = StagedWsMessage {
            message,
            payload_bytes,
        };
        match self.tx.try_send(staged) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(staged)) => {
                self.queued_bytes
                    .fetch_sub(staged.payload_bytes, Ordering::AcqRel);
                Err(WsIngressSendError::Full)
            }
            Err(mpsc::error::TrySendError::Closed(staged)) => {
                self.queued_bytes
                    .fetch_sub(staged.payload_bytes, Ordering::AcqRel);
                Err(WsIngressSendError::Closed)
            }
        }
    }
}

impl WsIngressReceiver {
    async fn recv(&mut self) -> Option<Message> {
        let staged = self.rx.recv().await?;
        self.queued_bytes
            .fetch_sub(staged.payload_bytes, Ordering::AcqRel);
        Some(staged.message)
    }

    fn try_recv(&mut self) -> Option<Message> {
        let staged = self.rx.try_recv().ok()?;
        self.queued_bytes
            .fetch_sub(staged.payload_bytes, Ordering::AcqRel);
        Some(staged.message)
    }
}

enum WsCommand {
    Send {
        message: Message,
        tx_result: oneshot::Sender<Result<(), WsError>>,
    },
}

impl WsStream {
    fn new(inner: WebSocketConnection) -> Self {
        let (tx_command, mut rx_command) = mpsc::channel::<WsCommand>(32);
        let (tx_message, rx_message) = ws_ingress_channel(
            WEBSOCKET_INGRESS_CAPACITY,
            WEBSOCKET_INGRESS_MAX_QUEUED_BYTES,
        );
        let (tx_failure, rx_failure) = oneshot::channel();

        let pump_task = tokio::spawn(async move {
            let mut inner = inner;
            let mut tx_failure = Some(tx_failure);
            loop {
                tokio::select! {
                    command = rx_command.recv() => {
                        let Some(command) = command else {
                            break;
                        };
                        match command {
                            WsCommand::Send { message, tx_result } => {
                                let result = inner.send(message).await;
                                let should_break = result.is_err();
                                let _ = tx_result.send(result);
                                if should_break {
                                    break;
                                }
                            }
                        }
                    }
                    message = inner.next() => {
                        let Some(message) = message else {
                            break;
                        };
                        match message {
                            Ok(Message::Ping(payload)) => {
                                if let Err(err) = inner.send(Message::Pong(payload)).await {
                                    if let Some(tx_failure) = tx_failure.take() {
                                        let _ = tx_failure.send(WsIngressFailure::Transport(err));
                                    }
                                    break;
                                }
                            }
                            Ok(Message::Pong(_)) => {}
                            Ok(message @ (Message::Text(_)
                            | Message::Binary(_)
                            | Message::Close(_)
                            | Message::Frame(_))) => {
                                let is_close = matches!(message, Message::Close(_));
                                match tx_message.try_send(message) {
                                    Ok(()) => {}
                                    Err(WsIngressSendError::Full) => {
                                        if let Some(tx_failure) = tx_failure.take() {
                                            let _ = tx_failure.send(WsIngressFailure::Overflow(
                                                WsError::Io(std::io::Error::other(
                                                    WEBSOCKET_INGRESS_OVERFLOW_MESSAGE,
                                                )),
                                            ));
                                        }
                                        break;
                                    }
                                    Err(WsIngressSendError::Closed) => break,
                                }
                                if is_close {
                                    break;
                                }
                            }
                            Err(err) => {
                                if let Some(tx_failure) = tx_failure.take() {
                                    let _ = tx_failure.send(WsIngressFailure::Transport(err));
                                }
                                break;
                            }
                        }
                    }
                }
            }
        });

        Self {
            tx_command,
            rx_message,
            rx_failure: Some(rx_failure),
            pending_failure: None,
            pump_task,
        }
    }

    async fn request(
        &self,
        make_command: impl FnOnce(oneshot::Sender<Result<(), WsError>>) -> WsCommand,
    ) -> Result<(), WsError> {
        let (tx_result, rx_result) = oneshot::channel();
        if self.tx_command.send(make_command(tx_result)).await.is_err() {
            return Err(WsError::ConnectionClosed);
        }
        rx_result.await.unwrap_or(Err(WsError::ConnectionClosed))
    }

    async fn send(&self, message: Message) -> Result<(), WsError> {
        self.request(|tx_result| WsCommand::Send { message, tx_result })
            .await
    }

    async fn next(&mut self) -> Option<Result<Message, WsError>> {
        loop {
            if let Some(error) = self.pending_failure.take() {
                if let Some(message) = self.rx_message.try_recv() {
                    self.pending_failure = Some(error);
                    return Some(Ok(message));
                }
                return Some(Err(error));
            }

            if let Some(rx_failure) = self.rx_failure.as_mut() {
                tokio::select! {
                    biased;
                    failure = rx_failure => {
                        self.rx_failure = None;
                        match failure {
                            Ok(WsIngressFailure::Overflow(error) | WsIngressFailure::Transport(error)) => {
                                self.pending_failure = Some(error);
                                continue;
                            }
                            Err(_) => {}
                        }
                    }
                    message = self.rx_message.recv() => return message.map(Ok),
                }
            } else {
                return self.rx_message.recv().await.map(Ok);
            }
        }
    }

    fn is_closed(&self) -> bool {
        self.pending_failure.is_some() || self.pump_task.is_finished()
    }
}

impl Drop for WsStream {
    fn drop(&mut self) {
        self.pump_task.abort();
    }
}

const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";
const WEBSOCKET_CONNECTION_LIMIT_REACHED_MESSAGE: &str = "Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue.";
const PREVIOUS_RESPONSE_NOT_FOUND_CODE: &str = "previous_response_not_found";
const PREVIOUS_RESPONSE_NOT_FOUND_MESSAGE: &str =
    "Previous response was not found. Retrying the full request.";

pub struct ResponsesWebsocketConnection {
    pump_handle: tokio::task::AbortHandle,
    retired: Arc<AtomicBool>,
    stream: Arc<Mutex<Option<WsStream>>>,
    // TODO (pakrym): is this the right place for timeout?
    idle_timeout: Duration,
    metadata: ResponsesStreamMetadata,
    telemetry: Option<Arc<dyn WebsocketTelemetry>>,
}

impl std::fmt::Debug for ResponsesWebsocketConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponsesWebsocketConnection")
            .field("stream", &"<ws-stream>")
            .field("idle_timeout", &self.idle_timeout)
            .field("metadata", &self.metadata)
            .field("telemetry", &self.telemetry.as_ref().map(|_| "<telemetry>"))
            .finish()
    }
}

impl ResponsesWebsocketConnection {
    fn new(
        stream: WsStream,
        idle_timeout: Duration,
        metadata: ResponsesStreamMetadata,
        telemetry: Option<Arc<dyn WebsocketTelemetry>>,
    ) -> Self {
        Self {
            pump_handle: stream.pump_task.abort_handle(),
            retired: Arc::new(AtomicBool::new(false)),
            stream: Arc::new(Mutex::new(Some(stream))),
            idle_timeout,
            metadata,
            telemetry,
        }
    }

    pub async fn is_closed(&self) -> bool {
        self.retired.load(Ordering::Acquire) || self.pump_handle.is_finished()
    }

    pub async fn stream_request(
        &self,
        request: ResponsesWsRequest,
        connection_reused: bool,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<ResponseStream, ApiError> {
        self.stream_request_with_dispatch_ready(
            &request,
            connection_reused,
            turn_state,
            || {},
            |_| {},
            || {},
        )
        .await
    }

    #[instrument(
        name = "responses_websocket.stream_request",
        level = "info",
        skip_all,
        fields(transport = "responses_websocket", api.path = "responses")
    )]
    pub async fn stream_request_with_dispatch_ready(
        &self,
        request: &ResponsesWsRequest,
        connection_reused: bool,
        turn_state: Option<Arc<OnceLock<String>>>,
        queue_started: impl FnOnce(),
        dispatch_ready: impl FnOnce(Bytes) + Send + 'static,
        stream_established: impl FnOnce() + Send + 'static,
    ) -> Result<ResponseStream, ApiError> {
        let (tx_event, rx_event) =
            mpsc::channel::<std::result::Result<ResponseEvent, ApiError>>(1600);
        let stream = Arc::clone(&self.stream);
        let retired = Arc::clone(&self.retired);
        let idle_timeout = self.idle_timeout;
        let metadata = self.metadata.clone();
        let upstream_request_id = metadata.upstream_request_id().map(str::to_string);
        let telemetry = self.telemetry.clone();
        let request_text = serialize_websocket_request(request)?;
        let encoded_request: Bytes = request_text.clone().into();
        let (tx_send_complete, rx_send_complete) = oneshot::channel();
        queue_started();

        let current_span = Span::current();
        tokio::spawn(
            #[expect(
                clippy::await_holding_invalid_type,
                reason = "the guard serializes exclusive use of the websocket stream for the lifetime of the response stream"
            )]
            async move {
                let mut guard = tokio::select! {
                    biased;
                    _ = tx_event.closed() => return,
                    guard = stream.lock() => guard,
                };
                // Abandon queued work without invalidating an unused connection.
                if tx_event.is_closed() {
                    return;
                }
                let result = 'response: {
                    let Some(ws_stream) = guard.as_mut() else {
                        let _ = tx_send_complete.send(());
                        let _ = tx_event
                            .send(Err(ApiError::Stream(
                                "websocket connection is closed".to_string(),
                            )))
                            .await;
                        return;
                    };

                    dispatch_ready(encoded_request);
                    let send_result = tokio::select! {
                        biased;
                        _ = tx_event.closed() => Err(ApiError::Stream(
                            "response event consumer dropped".to_string(),
                        )),
                        result = send_websocket_request(
                            ws_stream,
                            request_text,
                            idle_timeout,
                            telemetry.as_ref(),
                            connection_reused,
                        ) => result,
                    };
                    let send_succeeded = send_result.is_ok();
                    if send_succeeded {
                        stream_established();
                    }
                    let _ = tx_send_complete.send(());
                    if let Err(err) = send_result {
                        Err(err)
                    } else {
                        for event in metadata.initial_events() {
                            if tx_event.send(Ok(event)).await.is_err() {
                                break 'response Err(ApiError::Stream(
                                    "response event consumer dropped".to_string(),
                                ));
                            }
                        }

                        run_websocket_response_stream(
                            ws_stream,
                            tx_event.clone(),
                            idle_timeout,
                            telemetry,
                            metadata,
                            turn_state,
                        )
                        .await
                    }
                };

                if let Err(err) = result {
                    // A terminal stream error should reach the caller immediately. Waiting for a
                    // graceful close handshake here can stall indefinitely and mask the error.
                    retired.store(true, Ordering::Release);
                    let failed_stream = guard.take();
                    drop(guard);
                    drop(failed_stream);
                    let _ = tx_event.send(Err(err)).await;
                } else if guard.as_ref().is_some_and(WsStream::is_closed) {
                    retired.store(true, Ordering::Release);
                    let failed_stream = guard.take();
                    drop(guard);
                    drop(failed_stream);
                }
            }
            .instrument(current_span),
        );
        let _ = rx_send_complete.await;

        Ok(ResponseStream {
            rx_event,
            upstream_request_id,
        })
    }
}

/// Client for connecting to the Responses WebSocket endpoint for one provider.
pub struct ResponsesWebsocketClient {
    provider: Provider,
    auth: SharedAuthProvider,
}

/// Close frame information captured by a handshake probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesWebsocketClose {
    /// WebSocket close code returned by the server, absent for an empty close frame.
    pub code: Option<String>,
    /// Human-readable close reason returned by the server.
    pub reason: String,
}

/// Result of a handshake-only Responses WebSocket probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesWebsocketProbe {
    /// Redacted by callers before displaying or serializing support reports.
    pub url: String,
    /// HTTP status returned by the successful WebSocket upgrade.
    pub status: StatusCode,
    /// Whether the server reported reasoning support in the upgrade response.
    pub reasoning_included: bool,
    /// Whether the server returned a model catalog ETag in the upgrade response.
    pub models_etag_present: bool,
    /// Whether the server returned a server-selected model in the upgrade response.
    pub server_model_present: bool,
    /// Close frame received immediately after upgrade, when one arrives quickly.
    pub immediate_close: Option<ResponsesWebsocketClose>,
}

impl ResponsesWebsocketClient {
    /// Creates a Responses WebSocket client for an already-resolved provider and auth source.
    pub fn new(provider: Provider, auth: SharedAuthProvider) -> Self {
        Self { provider, auth }
    }

    #[instrument(
        name = "responses_websocket.connect",
        level = "info",
        skip_all,
        fields(transport = "responses_websocket", api.path = "responses")
    )]
    pub async fn connect(
        &self,
        http_client_factory: &HttpClientFactory,
        extra_headers: HeaderMap,
        default_headers: HeaderMap,
        turn_state: Option<Arc<OnceLock<String>>>,
        telemetry: Option<Arc<dyn WebsocketTelemetry>>,
    ) -> Result<ResponsesWebsocketConnection, ApiError> {
        let ws_url = self
            .provider
            .websocket_url_for_path("responses")
            .map_err(|err| ApiError::Stream(format!("failed to build websocket URL: {err}")))?;

        let mut headers =
            merge_request_headers(&self.provider.headers, extra_headers, default_headers);
        self.auth
            .try_add_auth_headers(&mut headers)
            .map_err(TransportError::from)?;

        let connected =
            connect_websocket(ws_url, headers, http_client_factory, turn_state.clone()).await?;
        Ok(ResponsesWebsocketConnection::new(
            connected.stream,
            self.provider.stream_idle_timeout,
            connected.metadata,
            telemetry,
        ))
    }

    /// Opens a WebSocket connection long enough to validate the upgrade response.
    ///
    /// The probe uses the same URL construction, headers, authentication, TLS,
    /// and custom-CA path as a real Responses WebSocket connection, but it does
    /// not send a request frame. After the HTTP 101 upgrade succeeds, it waits
    /// briefly for an immediate server close frame so diagnostics can distinguish
    /// a usable connection from a policy rejection that closes right away.
    pub async fn probe_handshake(
        &self,
        http_client_factory: &HttpClientFactory,
        extra_headers: HeaderMap,
        default_headers: HeaderMap,
        immediate_close_timeout: Duration,
    ) -> Result<ResponsesWebsocketProbe, ApiError> {
        let ws_url = self
            .provider
            .websocket_url_for_path("responses")
            .map_err(|err| ApiError::Stream(format!("failed to build websocket URL: {err}")))?;

        let mut headers =
            merge_request_headers(&self.provider.headers, extra_headers, default_headers);
        self.auth
            .try_add_auth_headers(&mut headers)
            .map_err(TransportError::from)?;

        let connected = connect_websocket(
            ws_url.clone(),
            headers,
            http_client_factory,
            /*turn_state*/ None,
        )
        .await?;
        let mut stream = connected.stream;
        let deadline = Instant::now() + immediate_close_timeout;
        let immediate_close = loop {
            if Instant::now() >= deadline {
                break None;
            }
            let message = match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(message)) => message.map_err(|err| {
                    ApiError::Stream(format!("failed to read websocket probe event: {err}"))
                })?,
                Ok(None) | Err(_) => break None,
            };
            if let Some(close) = immediate_close_from_message(message) {
                break Some(close);
            }
        };

        Ok(ResponsesWebsocketProbe {
            url: ws_url.to_string(),
            status: connected.status,
            reasoning_included: connected.metadata.reasoning_included(),
            models_etag_present: connected.metadata.models_etag_present(),
            server_model_present: connected.metadata.server_model_present(),
            immediate_close,
        })
    }
}

fn immediate_close_from_message(message: Message) -> Option<ResponsesWebsocketClose> {
    let Message::Close(frame) = message else {
        return None;
    };
    Some(
        frame
            .map(close_frame_to_probe)
            .unwrap_or(ResponsesWebsocketClose {
                code: None,
                reason: String::new(),
            }),
    )
}

fn close_frame_to_probe(frame: CloseFrame) -> ResponsesWebsocketClose {
    ResponsesWebsocketClose {
        code: Some(frame.code.to_string()),
        reason: frame.reason.to_string(),
    }
}

fn merge_request_headers(
    provider_headers: &HeaderMap,
    extra_headers: HeaderMap,
    default_headers: HeaderMap,
) -> HeaderMap {
    let mut headers = provider_headers.clone();
    headers.extend(extra_headers);
    for (name, value) in &default_headers {
        if let http::header::Entry::Vacant(entry) = headers.entry(name) {
            entry.insert(value.clone());
        }
    }
    headers
}

struct ConnectedWebsocket {
    stream: WsStream,
    status: StatusCode,
    metadata: ResponsesStreamMetadata,
}

async fn connect_websocket(
    url: Url,
    headers: HeaderMap,
    http_client_factory: &HttpClientFactory,
    turn_state: Option<Arc<OnceLock<String>>>,
) -> Result<ConnectedWebsocket, ApiError> {
    info!("connecting to responses websocket");

    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|err| ApiError::Stream(format!("failed to build websocket request: {err}")))?;
    request.headers_mut().extend(headers);

    let http_client_factory = http_client_factory.clone();
    let connector =
        tokio::task::spawn_blocking(move || WebSocketConnector::new(&http_client_factory))
            .await
            .map_err(|err| {
                ApiError::Stream(format!("websocket TLS configuration task failed: {err}"))
            })?
            .map_err(|err| ApiError::Stream(format!("failed to configure websocket TLS: {err}")))?
            .with_tcp_nodelay();
    let response = connector.connect(request, websocket_config()).await;

    let (stream, response) = match response {
        Ok((stream, response)) => {
            info!(
                status = %response.status(),
                "successfully connected to responses websocket"
            );
            (stream, response)
        }
        Err(err) => {
            error!("failed to connect to responses websocket");
            return Err(map_ws_error(err, &url));
        }
    };

    let metadata = ResponsesStreamMetadata::from_headers(response.headers());
    metadata.apply_turn_state(turn_state.as_deref());
    Ok(ConnectedWebsocket {
        stream: WsStream::new(stream),
        status: response.status(),
        metadata,
    })
}

fn websocket_config() -> WebSocketConfig {
    let mut extensions = ExtensionsConfig::default();
    extensions.permessage_deflate = Some(DeflateConfig::default());

    let mut config = WebSocketConfig::default();
    config.extensions = extensions;
    config
}

fn map_ws_error(err: WsError, url: &Url) -> ApiError {
    match err {
        WsError::Http(response) => {
            let status = response.status();
            let headers = response.headers().clone();
            let retry_after = codex_http_client::RetryAfter::from_headers(&headers);
            let body = response
                .body()
                .as_ref()
                .and_then(|bytes| String::from_utf8(bytes.clone()).ok());
            ApiError::Transport(TransportError::Http {
                status,
                url: Some(url.to_string()),
                headers: Some(headers),
                body,
                retry_after,
            })
        }
        WsError::ConnectionClosed | WsError::AlreadyClosed => {
            ApiError::Stream("websocket closed".to_string())
        }
        WsError::Io(err) => ApiError::Transport(TransportError::Network(err.to_string())),
        other => ApiError::Transport(TransportError::Network(other.to_string())),
    }
}

#[derive(Debug, Deserialize)]
struct WrappedWebsocketError {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WrappedWebsocketErrorEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(alias = "status_code")]
    status: Option<u16>,
    #[serde(default)]
    error: Option<WrappedWebsocketError>,
    #[serde(default)]
    headers: Option<JsonMap<String, Value>>,
}

fn parse_wrapped_websocket_error_event(payload: &str) -> Option<WrappedWebsocketErrorEvent> {
    let event: WrappedWebsocketErrorEvent = serde_json::from_str(payload).ok()?;
    if event.kind != "error" {
        return None;
    }
    Some(event)
}

fn map_wrapped_websocket_error_event(
    event: WrappedWebsocketErrorEvent,
    original_payload: String,
) -> Option<ApiError> {
    let WrappedWebsocketErrorEvent {
        status,
        error,
        headers,
        ..
    } = event;
    let headers = headers.as_ref().map(json_headers_to_http_headers);
    let retry_after = headers
        .as_ref()
        .and_then(codex_http_client::RetryAfter::from_headers);

    if let Some(error) = error.as_ref()
        && let Some(code) = error.code.as_deref()
        && let Some(fallback_message) = match code {
            WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE => {
                Some(WEBSOCKET_CONNECTION_LIMIT_REACHED_MESSAGE)
            }
            PREVIOUS_RESPONSE_NOT_FOUND_CODE => Some(PREVIOUS_RESPONSE_NOT_FOUND_MESSAGE),
            _ => None,
        }
    {
        return Some(ApiError::Retryable {
            message: error
                .message
                .clone()
                .unwrap_or_else(|| fallback_message.to_string()),
            delay: retry_after,
        });
    }

    let status = StatusCode::from_u16(status?).ok()?;
    if status.is_success() {
        return None;
    }

    Some(ApiError::Transport(TransportError::Http {
        status,
        url: None,
        headers,
        body: Some(original_payload),
        retry_after,
    }))
}

async fn run_websocket_response_stream(
    ws_stream: &mut WsStream,
    tx_event: mpsc::Sender<std::result::Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn WebsocketTelemetry>>,
    metadata: ResponsesStreamMetadata,
    turn_state: Option<Arc<OnceLock<String>>>,
) -> Result<(), ApiError> {
    let mut interpreter = ResponsesEventInterpreter::new(&metadata, turn_state);
    loop {
        let poll_start = Instant::now();
        let response = tokio::select! {
            biased;
            _ = tx_event.closed() => return Err(ApiError::Stream(
                "response event consumer dropped".to_string(),
            )),
            response = tokio::time::timeout(idle_timeout, ws_stream.next()) => response,
        }
        .map_err(|_| {
            ApiError::Stream(format!(
                "idle timeout waiting for websocket after {}ms",
                idle_timeout.as_millis()
            ))
        });
        if let Some(t) = telemetry.as_ref() {
            t.on_ws_event(&response, poll_start.elapsed());
        }
        let message = match response {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(err))) => return Err(websocket_read_error(err)),
            Ok(None) => {
                return Err(ApiError::Stream(
                    "stream closed before response.completed".into(),
                ));
            }
            Err(err) => {
                return Err(err);
            }
        };

        match message {
            Message::Text(text) => {
                let events = match interpreter.process_payload_with_error_mapper(&text, || {
                    let wrapped_error = parse_wrapped_websocket_error_event(&text)?;
                    map_wrapped_websocket_error_event(wrapped_error, text.to_string())
                }) {
                    Ok(events) => events,
                    Err(ResponsesEventError::Parse(error)) => {
                        debug!(
                            payload_bytes = text.len(),
                            category = ?error.classify(),
                            line = error.line(),
                            column = error.column(),
                            "failed to parse websocket event"
                        );
                        return Err(ApiError::Stream(
                            crate::responses_stream::decode_diagnostic(
                                &format!(
                                    "failed to parse websocket event ({} payload bytes)",
                                    text.len()
                                ),
                                &error,
                            ),
                        ));
                    }
                    Err(ResponsesEventError::Api(error)) => return Err(error),
                };
                for event in events {
                    let is_completed = matches!(event, ResponseEvent::Completed { .. });
                    if tx_event.send(Ok(event)).await.is_err() {
                        return Err(ApiError::Stream(
                            "response event consumer dropped".to_string(),
                        ));
                    }
                    if is_completed {
                        return Ok(());
                    }
                }
            }
            Message::Binary(_) => {
                return Err(ApiError::Stream("unexpected binary websocket event".into()));
            }
            Message::Close(frame) => {
                let mut message =
                    "websocket closed by server before response.completed".to_string();
                if let Some(frame) = frame {
                    message.push_str(&format!(
                        " (code {}, reason: {:?})",
                        frame.code,
                        frame.reason.as_str()
                    ));
                }
                return Err(ApiError::Stream(message));
            }
            Message::Frame(_) => {}
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }
}

fn websocket_read_error(err: WsError) -> ApiError {
    let message = match err {
        WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => {
            "websocket closed before response.completed".to_string()
        }
        err => err.to_string(),
    };
    ApiError::Stream(message)
}

async fn send_websocket_request(
    ws_stream: &WsStream,
    request_text: Utf8Bytes,
    idle_timeout: Duration,
    telemetry: Option<&Arc<dyn WebsocketTelemetry>>,
    connection_reused: bool,
) -> Result<(), ApiError> {
    let request_start = Instant::now();
    let result = tokio::time::timeout(idle_timeout, ws_stream.send(Message::Text(request_text)))
        .await
        .map_err(|_| {
            ApiError::Stream(format!(
                "idle timeout sending websocket request after {}ms",
                idle_timeout.as_millis()
            ))
        })
        .and_then(|result| {
            result
                .map_err(|err| ApiError::Stream(format!("failed to send websocket request: {err}")))
        });

    if let Some(t) = telemetry.as_ref() {
        t.on_ws_request(
            request_start.elapsed(),
            result.as_ref().err(),
            connection_reused,
        );
    }

    result?;

    Ok(())
}

fn serialize_websocket_request(request: &ResponsesWsRequest) -> Result<Utf8Bytes, ApiError> {
    serde_json::to_string(request)
        .map(Utf8Bytes::from)
        .map_err(|err| ApiError::Stream(format!("failed to encode websocket request: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::ResponseCreateWsRequest;
    use codex_protocol::ResponseItemId;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    #[tokio::test]
    async fn responses_connect_and_probe_reject_auth_before_network() {
        struct RejectedAuth;
        impl crate::auth::AuthProvider for RejectedAuth {
            fn add_auth_headers(&self, _: &mut HeaderMap) {}
            fn try_add_auth_headers(
                &self,
                _: &mut HeaderMap,
            ) -> Result<(), crate::auth::AuthError> {
                Err(crate::auth::AuthError::Build(
                    "credentials unavailable".into(),
                ))
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = ResponsesWebsocketClient::new(
            Provider {
                name: "auth-test".into(),
                base_url: format!("http://{}", listener.local_addr().unwrap()),
                query_params: None,
                headers: HeaderMap::new(),
                retry: crate::provider::RetryConfig {
                    max_retries: 0,
                    base_delay: Duration::ZERO,
                    retry_429: false,
                    retry_5xx: false,
                    retry_transport: false,
                },
                stream_idle_timeout: Duration::from_secs(1),
            },
            Arc::new(RejectedAuth),
        );
        let factory =
            HttpClientFactory::new(codex_http_client::OutboundProxyPolicy::ReqwestDefault);
        for probe in [false, true] {
            let result = tokio::time::timeout(Duration::from_secs(1), async {
                if probe {
                    client
                        .probe_handshake(
                            &factory,
                            HeaderMap::new(),
                            HeaderMap::new(),
                            Duration::from_millis(1),
                        )
                        .await
                        .map(|_| ())
                } else {
                    client
                        .connect(&factory, HeaderMap::new(), HeaderMap::new(), None, None)
                        .await
                        .map(|_| ())
                }
            })
            .await
            .expect("auth rejection must precede handshake");
            assert!(
                matches!(result, Err(ApiError::Transport(TransportError::Build(message))) if message == "credentials unavailable")
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    #[test]
    fn responses_connect_and_probe_cancel_tls_preparation_before_network() {
        struct NoAuth;
        impl crate::auth::AuthProvider for NoAuth {
            fn add_auth_headers(&self, _: &mut HeaderMap) {}
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");
        runtime.block_on(async {
            for probe in [false, true] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("listener");
                let client = ResponsesWebsocketClient::new(
                    Provider {
                        name: "tls-cancellation-test".to_string(),
                        base_url: format!("http://{}", listener.local_addr().expect("address")),
                        query_params: None,
                        headers: HeaderMap::new(),
                        retry: crate::provider::RetryConfig {
                            max_retries: 0,
                            base_delay: Duration::ZERO,
                            retry_429: false,
                            retry_5xx: false,
                            retry_transport: false,
                        },
                        stream_idle_timeout: Duration::from_secs(1),
                    },
                    Arc::new(NoAuth),
                );
                let factory =
                    HttpClientFactory::new(codex_http_client::OutboundProxyPolicy::ReqwestDefault);
                let (release, held) = std::sync::mpsc::channel::<()>();
                let (started, ready) = tokio::sync::oneshot::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    let _ = started.send(());
                    let _ = held.recv();
                });
                ready.await.expect("blocking worker started");
                let operation = async {
                    if probe {
                        client
                            .probe_handshake(
                                &factory,
                                HeaderMap::new(),
                                HeaderMap::new(),
                                Duration::from_secs(1),
                            )
                            .await
                            .map(|_| ())
                    } else {
                        client
                            .connect(&factory, HeaderMap::new(), HeaderMap::new(), None, None)
                            .await
                            .map(|_| ())
                    }
                };
                let result = tokio::time::timeout(Duration::from_millis(20), operation).await;
                drop(release);
                blocker.await.expect("worker released");
                assert!(
                    result.is_err(),
                    "TLS preparation must yield to cancellation"
                );
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), listener.accept())
                        .await
                        .is_err(),
                    "canceled CA preparation must not dispatch a websocket request"
                );
            }
        });
    }

    #[test]
    fn reset_without_close_handshake_maps_to_incomplete_stream_error() {
        let error = websocket_read_error(WsError::Protocol(
            ProtocolError::ResetWithoutClosingHandshake,
        ));
        let ApiError::Stream(message) = error else {
            panic!("expected stream error");
        };

        assert_eq!(message, "websocket closed before response.completed");
    }

    #[test]
    fn other_websocket_read_errors_keep_their_message() {
        let source = WsError::ConnectionClosed;
        let expected = source.to_string();
        let error = websocket_read_error(source);
        let ApiError::Stream(message) = error else {
            panic!("expected stream error");
        };

        assert_eq!(message, expected);
    }

    #[tokio::test]
    async fn connection_reports_closed_after_websocket_pump_exits() {
        let (tx_command, rx_command) = mpsc::channel::<WsCommand>(1);
        let (_tx_message, rx_message) = ws_ingress_channel(1, 1);
        let (tx_done, rx_done) = oneshot::channel();
        let pump_task = tokio::spawn(async move {
            let _rx_command = rx_command;
            let _ = rx_done.await;
        });
        let connection = ResponsesWebsocketConnection::new(
            WsStream {
                tx_command,
                rx_message,
                rx_failure: None,
                pending_failure: None,
                pump_task,
            },
            Duration::from_secs(1),
            ResponsesStreamMetadata::default(),
            None,
        );

        assert!(!connection.is_closed().await);
        tx_done.send(()).expect("websocket pump should be running");
        tokio::time::timeout(Duration::from_secs(1), async {
            while !connection.is_closed().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("websocket pump should stop");
    }

    #[tokio::test]
    async fn dispatch_callbacks_separate_socket_queue_from_transport_send() {
        let (tx_command, mut rx_command) = mpsc::channel::<WsCommand>(1);
        let (tx_message, rx_message) = ws_ingress_channel(1, 1024);
        let events = Arc::new(StdMutex::new(Vec::new()));
        let pump_events = Arc::clone(&events);
        let pump_task = tokio::spawn(async move {
            if let Some(WsCommand::Send { tx_result, .. }) = rx_command.recv().await {
                pump_events.lock().unwrap().push("sent");
                let _ = tx_result.send(Ok(()));
            }
        });
        let mut response_headers = HeaderMap::new();
        response_headers.insert("x-request-id", HeaderValue::from_static("ws-request-1"));
        let connection = ResponsesWebsocketConnection::new(
            WsStream {
                tx_command,
                rx_message,
                rx_failure: None,
                pending_failure: None,
                pump_task,
            },
            Duration::from_secs(1),
            ResponsesStreamMetadata::from_headers(&response_headers),
            None,
        );
        let request = ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest {
            model: "gpt-test".to_string(),
            instructions: String::new(),
            previous_response_id: None,
            input: Vec::new().into(),
            tools: None,
            tool_choice: "auto".to_string(),
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            stream_options: None,
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            generate: None,
            client_metadata: None,
        });
        let queue_events = Arc::clone(&events);
        let dispatch_events = Arc::clone(&events);
        let established_events = Arc::clone(&events);
        let expected_request = serialize_websocket_request(&request).unwrap();

        let stream = connection
            .stream_request_with_dispatch_ready(
                &request,
                false,
                None,
                move || queue_events.lock().unwrap().push("queue"),
                move |request_bytes| {
                    assert_eq!(request_bytes.as_ref(), expected_request.as_bytes());
                    dispatch_events.lock().unwrap().push("dispatch");
                },
                move || established_events.lock().unwrap().push("established"),
            )
            .await
            .expect("request should reach the websocket pump");

        assert_eq!(stream.upstream_request_id.as_deref(), Some("ws-request-1"));
        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["queue", "dispatch", "sent", "established"]
        );
        drop(stream);
        drop(tx_message);
    }

    #[tokio::test]
    async fn websocket_ingress_enforces_item_and_byte_limits() {
        let (item_tx, mut item_rx) = ws_ingress_channel(1, 1024);
        item_tx
            .try_send(Message::Text("first".into()))
            .expect("first item should fit");
        assert_eq!(
            item_tx.try_send(Message::Text("second".into())),
            Err(WsIngressSendError::Full)
        );
        assert_eq!(item_rx.recv().await, Some(Message::Text("first".into())));
        item_tx
            .try_send(Message::Text("second".into()))
            .expect("capacity should be released after receive");

        let (byte_tx, mut byte_rx) = ws_ingress_channel(2, 3);
        byte_tx
            .try_send(Message::Text("ab".into()))
            .expect("first payload should fit byte budget");
        assert_eq!(
            byte_tx.try_send(Message::Text("cd".into())),
            Err(WsIngressSendError::Full)
        );
        assert_eq!(byte_rx.recv().await, Some(Message::Text("ab".into())));
        byte_tx
            .try_send(Message::Text("cd".into()))
            .expect("byte budget should be released after receive");
    }

    #[tokio::test]
    async fn caller_dropped_during_send_closes_socket_before_metadata_delivery() {
        let (tx_command, mut rx_command) = mpsc::channel::<WsCommand>(1);
        let (_tx_message, rx_message) = ws_ingress_channel(1, 1024);
        let (tx_dispatched, rx_dispatched) = oneshot::channel();
        let (mut tx_release_send, rx_release_send) = oneshot::channel::<()>();
        let dispatches = Arc::new(AtomicUsize::new(0));
        let observed_dispatches = Arc::clone(&dispatches);
        let pump_task = tokio::spawn(async move {
            let Some(WsCommand::Send { tx_result, .. }) = rx_command.recv().await else {
                panic!("expected first dispatch");
            };
            observed_dispatches.fetch_add(1, Ordering::SeqCst);
            tx_dispatched.send(()).expect("dispatch observer");
            rx_release_send.await.expect("release external send");
            let _ = tx_result.send(Ok(()));
            while let Some(WsCommand::Send { tx_result, .. }) = rx_command.recv().await {
                observed_dispatches.fetch_add(1, Ordering::SeqCst);
                let _ = tx_result.send(Ok(()));
            }
        });
        let mut headers = HeaderMap::new();
        headers.insert("openai-model", HeaderValue::from_static("server-model"));
        let connection = Arc::new(ResponsesWebsocketConnection::new(
            WsStream {
                tx_command,
                rx_message,
                rx_failure: None,
                pending_failure: None,
                pump_task,
            },
            Duration::from_secs(60),
            ResponsesStreamMetadata::from_headers(&headers),
            None,
        ));
        let request = Arc::new(ResponsesWsRequest::ResponseCreate(
            ResponseCreateWsRequest {
                model: "gpt-test".to_string(),
                instructions: String::new(),
                previous_response_id: None,
                input: Vec::new().into(),
                tools: None,
                tool_choice: "auto".to_string(),
                parallel_tool_calls: true,
                reasoning: None,
                store: false,
                stream: true,
                stream_options: None,
                include: Vec::new(),
                service_tier: None,
                prompt_cache_key: None,
                text: None,
                generate: None,
                client_metadata: None,
            },
        ));
        let caller_connection = Arc::clone(&connection);
        let caller_request = Arc::clone(&request);
        let caller = tokio::spawn(async move {
            caller_connection
                .stream_request_with_dispatch_ready(
                    &caller_request,
                    false,
                    None,
                    || {},
                    |_| {},
                    || {},
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), rx_dispatched)
            .await
            .expect("request must dispatch")
            .expect("dispatch signal");
        caller.abort();
        assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
        tokio::time::timeout(Duration::from_secs(1), tx_release_send.closed())
            .await
            .expect("cancellation must stop the pump before send completes");

        assert!(
            tokio::time::timeout(Duration::from_secs(1), connection.is_closed())
                .await
                .expect("cancellation must release connection lock")
        );
        let mut second = connection
            .stream_request_with_dispatch_ready(&request, true, None, || {}, |_| {}, || {})
            .await
            .expect("closed connection reports through response stream");
        let error = second
            .next()
            .await
            .expect("closed connection error")
            .expect_err("must reject reuse");
        assert!(
            matches!(error, ApiError::Stream(message) if message == "websocket connection is closed")
        );
        assert!(second.next().await.is_none());
        assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Hold the active response lock across cancellation to prove a queued caller cannot dispatch"
    )]
    async fn canceled_queued_request_leaves_connection_reusable_without_dispatch() {
        let (tx_command, mut rx_command) = mpsc::channel::<WsCommand>(1);
        let (tx_message, rx_message) = ws_ingress_channel(2, 1024);
        let dispatches = Arc::new(AtomicUsize::new(0));
        let observed_dispatches = Arc::clone(&dispatches);
        let pump_task = tokio::spawn(async move {
            while let Some(WsCommand::Send { message, tx_result }) = rx_command.recv().await {
                observed_dispatches.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    serde_json::from_str::<Value>(message.to_text().unwrap()).unwrap()["model"],
                    "live-request"
                );
                let _ = tx_result.send(Ok(()));
                tx_message
                    .try_send(Message::Text(
                        json!({"type": "response.completed", "response": {"id": "live-response"}})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
            }
        });
        let connection = ResponsesWebsocketConnection::new(
            WsStream {
                tx_command,
                rx_message,
                rx_failure: None,
                pending_failure: None,
                pump_task,
            },
            Duration::from_secs(60),
            ResponsesStreamMetadata::default(),
            None,
        );
        // Hold the same lock an active response owns while another request queues.
        let guard = connection.stream.lock().await;
        let abandoned_request = test_response_request("abandoned-request");
        let mut abandoned = Box::pin(connection.stream_request(abandoned_request, true, None));
        assert!(futures::poll!(&mut abandoned).is_pending());
        tokio::task::yield_now().await;
        drop(abandoned);
        drop(guard);

        let mut live = tokio::time::timeout(
            Duration::from_secs(1),
            connection.stream_request(test_response_request("live-request"), true, None),
        )
        .await
        .expect("live request must acquire the reusable connection")
        .unwrap();
        assert!(
            matches!(live.next().await, Some(Ok(ResponseEvent::RateLimits(snapshot)))
            if snapshot.limit_id.as_deref() == Some("codex"))
        );
        let event = tokio::time::timeout(Duration::from_secs(1), live.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            matches!(event, ResponseEvent::Completed { response_id, .. } if response_id == "live-response")
        );
        assert_eq!(dispatches.load(Ordering::SeqCst), 1);
        assert!(!connection.is_closed().await);
    }

    #[tokio::test]
    async fn consumer_dropped_while_waiting_for_frame_closes_socket_without_metadata() {
        let (tx_command, mut rx_command) = mpsc::channel::<WsCommand>(1);
        let (_tx_message, rx_message) = ws_ingress_channel(1, 1024);
        let (mut tx_pump_lifetime, rx_pump_lifetime) = oneshot::channel::<()>();
        let pump_task = tokio::spawn(async move {
            let Some(WsCommand::Send { tx_result, .. }) = rx_command.recv().await else {
                panic!("request must dispatch");
            };
            tx_result.send(Ok(())).unwrap();
            let _ = rx_pump_lifetime.await;
        });
        let connection = ResponsesWebsocketConnection::new(
            WsStream {
                tx_command,
                rx_message,
                rx_failure: None,
                pending_failure: None,
                pump_task,
            },
            Duration::from_secs(60),
            ResponsesStreamMetadata::default(),
            None,
        );
        let mut response = connection
            .stream_request(test_response_request("gpt-test"), false, None)
            .await
            .unwrap();
        assert!(
            matches!(response.next().await, Some(Ok(ResponseEvent::RateLimits(snapshot)))
            if snapshot.limit_id.as_deref() == Some("codex"))
        );
        assert!(futures::poll!(response.next()).is_pending());
        drop(response);
        tokio::time::timeout(Duration::from_secs(1), tx_pump_lifetime.closed())
            .await
            .expect("consumer loss must dispose of the idle socket promptly");
        assert!(connection.is_closed().await);
    }

    fn test_response_request(model: &str) -> ResponsesWsRequest {
        ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest {
            model: model.to_string(),
            instructions: String::new(),
            previous_response_id: None,
            input: Vec::new().into(),
            tools: None,
            tool_choice: "auto".to_string(),
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            stream_options: None,
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            generate: None,
            client_metadata: None,
        })
    }

    #[tokio::test]
    async fn handshake_probe_observes_close_after_text_with_or_without_status() {
        struct NoAuth;
        impl crate::auth::AuthProvider for NoAuth {
            fn add_auth_headers(&self, _: &mut HeaderMap) {}
        }
        for frame in [
            Some(CloseFrame {
                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy,
                reason: "policy rejection".into(),
            }),
            None,
        ] {
            let expected = ResponsesWebsocketClose {
                code: frame.as_ref().map(|frame| frame.code.to_string()),
                reason: frame
                    .as_ref()
                    .map(|frame| frame.reason.to_string())
                    .unwrap_or_default(),
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut websocket =
                    tokio_tungstenite::accept_async_with_config(stream, Some(websocket_config()))
                        .await
                        .unwrap();
                websocket.send(Message::Text("hello".into())).await.unwrap();
                websocket.send(Message::Close(frame)).await.unwrap();
            });
            let client = ResponsesWebsocketClient::new(
                Provider {
                    name: "probe-test".to_string(),
                    base_url: format!("http://{address}"),
                    query_params: None,
                    headers: HeaderMap::new(),
                    retry: crate::provider::RetryConfig {
                        max_retries: 0,
                        base_delay: Duration::ZERO,
                        retry_429: false,
                        retry_5xx: false,
                        retry_transport: false,
                    },
                    stream_idle_timeout: Duration::from_secs(1),
                },
                Arc::new(NoAuth),
            );
            let factory =
                HttpClientFactory::new(codex_http_client::OutboundProxyPolicy::ReqwestDefault);
            let probe = tokio::time::timeout(
                Duration::from_secs(5),
                client.probe_handshake(
                    &factory,
                    HeaderMap::new(),
                    HeaderMap::new(),
                    Duration::from_secs(1),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(probe.status, StatusCode::SWITCHING_PROTOCOLS);
            assert_eq!(probe.immediate_close, Some(expected));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn websocket_ingress_failure_follows_staged_messages() {
        let (tx_command, _rx_command) = mpsc::channel::<WsCommand>(1);
        let (tx_message, rx_message) = ws_ingress_channel(1, 1024);
        tx_message
            .try_send(Message::Text("queued".into()))
            .expect("message should be staged");
        let (tx_failure, rx_failure) = oneshot::channel();
        tx_failure
            .send(WsIngressFailure::Overflow(WsError::Io(
                std::io::Error::other(WEBSOCKET_INGRESS_OVERFLOW_MESSAGE),
            )))
            .expect("failure receiver should be open");
        let pump_task = tokio::spawn(async {});
        let mut stream = WsStream {
            tx_command,
            rx_message,
            rx_failure: Some(rx_failure),
            pending_failure: None,
            pump_task,
        };

        assert_eq!(
            stream
                .next()
                .await
                .expect("accepted prefix")
                .expect("queued message"),
            Message::Text("queued".into())
        );
        assert!(
            stream.is_closed(),
            "failure-marked connection must not be reusable"
        );
        let error = stream
            .next()
            .await
            .expect("failure should be emitted")
            .expect_err("overflow should fail the stream");
        assert!(
            error
                .to_string()
                .contains(WEBSOCKET_INGRESS_OVERFLOW_MESSAGE)
        );
    }

    #[tokio::test]
    async fn websocket_ingress_transport_failure_follows_staged_messages() {
        let (tx_command, _rx_command) = mpsc::channel::<WsCommand>(1);
        let (tx_message, rx_message) = ws_ingress_channel(1, 1024);
        let queued = Message::Text("queued".into());
        tx_message
            .try_send(queued.clone())
            .expect("message should be staged");
        let (tx_failure, rx_failure) = oneshot::channel();
        tx_failure
            .send(WsIngressFailure::Transport(WsError::ConnectionClosed))
            .expect("failure receiver should be open");
        let mut stream = WsStream {
            tx_command,
            rx_message,
            rx_failure: Some(rx_failure),
            pending_failure: None,
            pump_task: tokio::spawn(async {}),
        };

        let message = stream
            .next()
            .await
            .expect("message should be emitted")
            .expect("staged message should precede the transport failure");
        assert_eq!(message, queued);
        assert!(matches!(
            stream.next().await.expect("failure should be emitted"),
            Err(WsError::ConnectionClosed)
        ));
    }

    #[tokio::test]
    async fn audit_overflow_preserves_only_a_valid_accepted_completion_and_retires_connection() {
        let completed =
            json!({"type":"response.completed","response":{"id":"accepted"}}).to_string();
        let delta = json!({"type":"response.output_text.delta","delta":"partial"}).to_string();
        let malformed =
            json!({"type":"response.output_item.done","item":{"type":"message"}}).to_string();
        for (prefix, success) in [
            (vec![delta.clone(), completed.clone()], true),
            (vec![delta.clone()], false),
            (vec![malformed, completed.clone()], false),
        ] {
            let (tx_command, mut rx_command) = mpsc::channel::<WsCommand>(1);
            let (tx_message, rx_message) = ws_ingress_channel(prefix.len(), 4096);
            for frame in prefix {
                tx_message.try_send(Message::Text(frame.into())).unwrap();
            }
            assert_eq!(
                tx_message.try_send(Message::Text(completed.clone().into())),
                Err(WsIngressSendError::Full)
            );
            let (tx_failure, rx_failure) = oneshot::channel();
            tx_failure
                .send(WsIngressFailure::Overflow(WsError::Io(
                    std::io::Error::other(WEBSOCKET_INGRESS_OVERFLOW_MESSAGE),
                )))
                .unwrap();
            drop(tx_message);
            let pump_task = tokio::spawn(async move {
                if let Some(WsCommand::Send { tx_result, .. }) = rx_command.recv().await {
                    let _ = tx_result.send(Ok(()));
                }
                std::future::pending::<()>().await;
            });
            let connection = ResponsesWebsocketConnection::new(
                WsStream {
                    tx_command,
                    rx_message,
                    rx_failure: Some(rx_failure),
                    pending_failure: None,
                    pump_task,
                },
                Duration::from_secs(1),
                ResponsesStreamMetadata::default(),
                None,
            );
            // Health inspection is independent of active response ownership.
            let guard = connection.stream.lock().await;
            assert!(
                !tokio::time::timeout(Duration::from_millis(100), connection.is_closed())
                    .await
                    .unwrap()
            );
            drop(guard);
            let mut stream = connection
                .stream_request(test_response_request("test"), false, None)
                .await
                .unwrap();
            let mut completions = 0;
            let mut errors = 0;
            while let Some(event) = tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .unwrap()
            {
                match event {
                    Ok(ResponseEvent::Completed { .. }) => completions += 1,
                    Err(_) => errors += 1,
                    _ => {}
                }
            }
            assert_eq!(completions, usize::from(success));
            assert_eq!(errors, usize::from(!success));
            assert!(connection.is_closed().await);
        }
    }

    #[tokio::test]
    async fn protocol_errors_stop_before_completed() {
        let completed = json!({
            "type": "response.completed",
            "response": {"id": "resp1"}
        })
        .to_string();

        for (case, payload) in [
            (
                "invalid output item",
                json!({
                    "type": "response.output_item.done",
                    "item": {"type": "message"}
                })
                .to_string(),
            ),
            (
                "malformed json",
                r#"{"type":"response.output_item.done""#.to_string(),
            ),
        ] {
            let (tx_command, _rx_command) = mpsc::channel::<WsCommand>(1);
            let (tx_message, rx_message) = ws_ingress_channel(2, 1024);
            tx_message
                .try_send(Message::Text(payload.clone().into()))
                .expect("invalid event should fit ingress queue");
            tx_message
                .try_send(Message::Text(completed.clone().into()))
                .expect("completion should fit ingress queue");
            drop(tx_message);
            let mut ws_stream = WsStream {
                tx_command,
                rx_message,
                rx_failure: None,
                pending_failure: None,
                pump_task: tokio::spawn(std::future::pending()),
            };
            let (tx_event, mut rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(2);

            let error = run_websocket_response_stream(
                &mut ws_stream,
                tx_event,
                Duration::from_secs(1),
                /*telemetry*/ None,
                ResponsesStreamMetadata::default(),
                /*turn_state*/ None,
            )
            .await
            .expect_err("protocol error should terminate before completion");

            match error {
                ApiError::Stream(message) => {
                    assert!(
                        message.contains("response.output_item.done")
                            || message.contains("failed to parse websocket event"),
                        "case {case}: {message}"
                    );
                    if case == "malformed json" {
                        assert!(message.contains(&format!("({} payload bytes)", payload.len())));
                        assert!(message.contains("Eof at line 1 column"));
                    }
                }
                other => panic!("unexpected error for {case}: {other:?}"),
            }
            assert!(
                rx_event.recv().await.is_none(),
                "case {case} emitted a response event"
            );
        }
    }

    #[tokio::test]
    async fn wrapped_errors_preempt_completion_even_with_incompatible_response_fields() {
        for payload in [
            r#"{"type":"error","status":429,"error":{"message":"slow down"},"headers":{"retry-after":"3"}}"#,
            r#"{"response":false,"status_code":429,"headers":{"retry-after":"3"},"type":"\u0065rror"}"#,
        ] {
            let (tx_command, _rx_command) = mpsc::channel::<WsCommand>(1);
            let (tx_message, rx_message) = ws_ingress_channel(2, 2048);
            tx_message
                .try_send(Message::Text(payload.into()))
                .expect("error fits ingress queue");
            tx_message
                .try_send(Message::Text(
                    r#"{"type":"response.completed","response":{"id":"resp1"}}"#.into(),
                ))
                .expect("completion fits ingress queue");
            let mut ws_stream = WsStream {
                tx_command,
                rx_message,
                rx_failure: None,
                pending_failure: None,
                pump_task: tokio::spawn(std::future::pending()),
            };
            let (tx_event, mut rx_event) = mpsc::channel(2);
            let error = run_websocket_response_stream(
                &mut ws_stream,
                tx_event,
                Duration::from_secs(1),
                None,
                ResponsesStreamMetadata::default(),
                None,
            )
            .await
            .expect_err("wrapped error must terminate the stream");
            let ApiError::Transport(TransportError::Http {
                status,
                headers,
                body,
                ..
            }) = error
            else {
                panic!("expected the wrapped HTTP error, got {error:?}");
            };
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(headers.expect("error headers")["retry-after"], "3");
            assert_eq!(body.as_deref(), Some(payload));
            assert!(
                rx_event.recv().await.is_none(),
                "completion must not escape"
            );
        }
    }

    #[tokio::test]
    async fn response_stream_preserves_server_close_details() {
        for (frame, expected) in [
            (
                Some(CloseFrame {
                    code:
                        tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy,
                    reason: "policy rejection".into(),
                }),
                "websocket closed by server before response.completed (code 1008, reason: \"policy rejection\")",
            ),
            (None, "websocket closed by server before response.completed"),
        ] {
            let (tx_command, mut rx_command) = mpsc::channel::<WsCommand>(1);
            let (tx_message, rx_message) = ws_ingress_channel(2, 1024);
            let pump_task = tokio::spawn(async move {
                let Some(WsCommand::Send { tx_result, .. }) = rx_command.recv().await else {
                    panic!("response request must dispatch");
                };
                tx_result.send(Ok(())).unwrap();
                tx_message.try_send(Message::Close(frame)).unwrap();
                tx_message.try_send(Message::Text(
                    json!({"type": "response.completed", "response": {"id": "must-not-complete"}})
                        .to_string().into(),
                )).unwrap();
                std::future::pending::<()>().await;
            });
            let connection = ResponsesWebsocketConnection::new(
                WsStream {
                    tx_command,
                    rx_message,
                    rx_failure: None,
                    pending_failure: None,
                    pump_task,
                },
                Duration::from_secs(1),
                ResponsesStreamMetadata::default(),
                None,
            );
            let mut response = connection
                .stream_request(test_response_request("test-model"), false, None)
                .await
                .unwrap();
            assert!(
                matches!(response.next().await, Some(Ok(ResponseEvent::RateLimits(snapshot)))
                if snapshot.limit_id.as_deref() == Some("codex"))
            );
            let error = response.next().await.unwrap().unwrap_err();
            assert!(
                matches!(&error, ApiError::Stream(message) if message == expected),
                "{error:?}"
            );
            assert!(
                response.next().await.is_none(),
                "close must prevent completion"
            );
            assert!(
                connection.is_closed().await,
                "failed socket must not be reused"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn response_stream_reports_send_and_receive_timeout_durations() {
        for send_stalls in [true, false] {
            let (tx_command, mut rx_command) = mpsc::channel::<WsCommand>(1);
            let (tx_message, rx_message) = ws_ingress_channel(1, 1024);
            let pump_task = tokio::spawn(async move {
                let Some(WsCommand::Send { tx_result, .. }) = rx_command.recv().await else {
                    panic!("response request must dispatch");
                };
                if send_stalls {
                    // Keep the request acknowledgement open until the client times out.
                    let _tx_result = tx_result;
                    std::future::pending::<()>().await;
                } else {
                    tx_result.send(Ok(())).unwrap();
                }
                let _tx_message = tx_message;
                std::future::pending::<()>().await;
            });
            let connection = ResponsesWebsocketConnection::new(
                WsStream {
                    tx_command,
                    rx_message,
                    rx_failure: None,
                    pending_failure: None,
                    pump_task,
                },
                Duration::from_millis(1250),
                ResponsesStreamMetadata::default(),
                None,
            );
            let mut response = connection
                .stream_request(test_response_request("test-model"), false, None)
                .await
                .unwrap();
            if !send_stalls {
                assert!(
                    matches!(response.next().await, Some(Ok(ResponseEvent::RateLimits(snapshot)))
                    if snapshot.limit_id.as_deref() == Some("codex"))
                );
            }
            let error = response.next().await.unwrap().unwrap_err();
            let expected = if send_stalls {
                "idle timeout sending websocket request after 1250ms"
            } else {
                "idle timeout waiting for websocket after 1250ms"
            };
            assert!(
                matches!(&error, ApiError::Stream(message) if message == expected),
                "{error:?}"
            );
            assert!(response.next().await.is_none());
            assert!(connection.is_closed().await);
        }
    }

    #[test]
    fn direct_serialization_preserves_websocket_request_payload() {
        let request = ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest {
            model: "gpt-test".to_string(),
            instructions: "Use the available tools.".to_string(),
            previous_response_id: Some("resp-1".to_string()),
            input: vec![ResponseItem::Message {
                id: Some(ResponseItemId::with_suffix("msg", "1")),
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "hello".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }]
            .into(),
            tools: Some(
                vec![json!({
                    "type": "function",
                    "name": "lookup",
                    "parameters": {"type": "object"}
                })]
                .into(),
            ),
            tool_choice: "auto".to_string(),
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            stream_options: None,
            include: vec!["reasoning.encrypted_content".to_string()],
            service_tier: Some("priority".to_string()),
            prompt_cache_key: Some("cache-key".to_string()),
            text: None,
            generate: Some(false),
            client_metadata: Some(HashMap::from([(
                "traceparent".to_string(),
                "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01".to_string(),
            )])),
        });

        let request_text =
            serialize_websocket_request(&request).expect("serialize websocket request");
        let wire_payload =
            serde_json::from_str::<Value>(&request_text).expect("parse websocket request");

        assert_eq!(
            wire_payload,
            json!({
                "type": "response.create",
                "model": "gpt-test",
                "instructions": "Use the available tools.",
                "previous_response_id": "resp-1",
                "input": [{
                    "type": "message",
                    "id": "msg_1",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hello"}],
                }],
                "tools": [{
                    "type": "function",
                    "name": "lookup",
                    "parameters": {"type": "object"},
                }],
                "tool_choice": "auto",
                "parallel_tool_calls": true,
                "reasoning": null,
                "store": false,
                "stream": true,
                "include": ["reasoning.encrypted_content"],
                "service_tier": "priority",
                "prompt_cache_key": "cache-key",
                "generate": false,
                "client_metadata": {
                    "traceparent": "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01",
                },
            })
        );
    }

    #[test]
    fn websocket_config_enables_permessage_deflate() {
        let config = websocket_config();
        assert!(config.extensions.permessage_deflate.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn parse_wrapped_websocket_error_event_maps_to_transport_http() {
        let payload = json!({
            "type": "error",
            "status": 429,
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached",
                "plan_type": "pro",
                "resets_at": 1738888888
            },
            "headers": {
                "x-codex-primary-used-percent": "100.0",
                "x-codex-primary-window-minutes": 15,
                "retry-after": "7"
            }
        })
        .to_string();

        let wrapped_error = parse_wrapped_websocket_error_event(&payload)
            .expect("expected websocket error payload to be parsed");
        let api_error = map_wrapped_websocket_error_event(wrapped_error, payload)
            .expect("expected websocket error payload to map to ApiError");

        let expected_deadline = tokio::time::Instant::now() + Duration::from_secs(7);
        let ApiError::Transport(TransportError::Http {
            status,
            headers,
            body,
            retry_after,
            ..
        }) = api_error
        else {
            panic!("expected ApiError::Transport(Http)");
        };

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(retry_after.unwrap().deadline(), expected_deadline);
        let headers = headers.expect("expected headers");
        assert_eq!(
            headers
                .get("x-codex-primary-used-percent")
                .and_then(|value| value.to_str().ok()),
            Some("100.0")
        );
        assert_eq!(
            headers
                .get("x-codex-primary-window-minutes")
                .and_then(|value| value.to_str().ok()),
            Some("15")
        );
        let body = body.expect("expected body");
        assert!(body.contains("usage_limit_reached"));
        assert!(body.contains("The usage limit has been reached"));
    }

    #[test]
    fn parse_wrapped_websocket_error_event_ignores_non_error_payloads() {
        let payload = json!({
            "type": "response.created",
            "response": {
                "id": "resp-1"
            }
        })
        .to_string();

        let wrapped_error = parse_wrapped_websocket_error_event(&payload);
        assert!(wrapped_error.is_none());
    }

    #[test]
    fn parse_wrapped_websocket_error_event_with_status_maps_invalid_request() {
        let payload = json!({
            "type": "error",
            "status": 400,
            "error": {
                "type": "invalid_request_error",
                "message": "Model does not support image inputs"
            }
        })
        .to_string();

        let wrapped_error = parse_wrapped_websocket_error_event(&payload)
            .expect("expected websocket error payload to be parsed");
        let api_error = map_wrapped_websocket_error_event(wrapped_error, payload)
            .expect("expected websocket error payload to map to ApiError");
        let ApiError::Transport(TransportError::Http { status, body, .. }) = api_error else {
            panic!("expected ApiError::Transport(Http)");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body = body.expect("expected body");
        assert!(body.contains("invalid_request_error"));
        assert!(body.contains("Model does not support image inputs"));
    }

    #[test]
    fn parse_wrapped_websocket_error_event_with_connection_limit_maps_retryable() {
        let payload = json!({
            "type": "error",
            "status": 400,
            "error": {
                "type": "invalid_request_error",
                "code": "websocket_connection_limit_reached",
                "message": "Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue."
            }
        })
        .to_string();

        let wrapped_error = parse_wrapped_websocket_error_event(&payload)
            .expect("expected websocket error payload to be parsed");
        let api_error = map_wrapped_websocket_error_event(wrapped_error, payload)
            .expect("expected websocket error payload to map to ApiError");
        let ApiError::Retryable { message, delay } = api_error else {
            panic!("expected ApiError::Retryable");
        };
        assert_eq!(message, WEBSOCKET_CONNECTION_LIMIT_REACHED_MESSAGE);
        assert_eq!(delay, None);
    }

    #[test]
    fn parse_wrapped_websocket_error_event_without_status_is_not_mapped() {
        let payload = json!({
            "type": "error",
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached"
            },
            "headers": {
                "x-codex-primary-used-percent": "100.0",
                "x-codex-primary-window-minutes": 15
            }
        })
        .to_string();

        let wrapped_error = parse_wrapped_websocket_error_event(&payload)
            .expect("expected websocket error payload to be parsed");
        let api_error = map_wrapped_websocket_error_event(wrapped_error, payload);
        assert!(api_error.is_none());
    }

    #[test]
    fn merge_request_headers_matches_http_precedence() {
        let mut provider_headers = HeaderMap::new();
        provider_headers.insert(
            "originator",
            HeaderValue::from_static("provider-originator"),
        );
        provider_headers.insert("x-priority", HeaderValue::from_static("provider"));

        let mut extra_headers = HeaderMap::new();
        extra_headers.insert("x-priority", HeaderValue::from_static("extra"));

        let mut default_headers = HeaderMap::new();
        default_headers.insert("originator", HeaderValue::from_static("default-originator"));
        default_headers.insert("x-priority", HeaderValue::from_static("default"));
        default_headers.insert("x-default-only", HeaderValue::from_static("default-only"));

        let merged = merge_request_headers(&provider_headers, extra_headers, default_headers);

        assert_eq!(
            merged.get("originator"),
            Some(&HeaderValue::from_static("provider-originator"))
        );
        assert_eq!(
            merged.get("x-priority"),
            Some(&HeaderValue::from_static("extra"))
        );
        assert_eq!(
            merged.get("x-default-only"),
            Some(&HeaderValue::from_static("default-only"))
        );
    }
}
