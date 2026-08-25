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
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
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
    rx_message: mpsc::UnboundedReceiver<Result<Message, WsError>>,
    pump_task: tokio::task::JoinHandle<()>,
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
        let (tx_message, rx_message) = mpsc::unbounded_channel::<Result<Message, WsError>>();

        let pump_task = tokio::spawn(async move {
            let mut inner = inner;
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
                                    let _ = tx_message.send(Err(err));
                                    break;
                                }
                            }
                            Ok(Message::Pong(_)) => {}
                            Ok(message @ (Message::Text(_)
                            | Message::Binary(_)
                            | Message::Close(_)
                            | Message::Frame(_))) => {
                                let is_close = matches!(message, Message::Close(_));
                                if tx_message.send(Ok(message)).is_err() {
                                    break;
                                }
                                if is_close {
                                    break;
                                }
                            }
                            Err(err) => {
                                let _ = tx_message.send(Err(err));
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
        self.rx_message.recv().await
    }

    fn is_closed(&self) -> bool {
        self.pump_task.is_finished()
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
            stream: Arc::new(Mutex::new(Some(stream))),
            idle_timeout,
            metadata,
            telemetry,
        }
    }

    pub async fn is_closed(&self) -> bool {
        match self.stream.lock().await.as_ref() {
            Some(stream) => stream.is_closed(),
            None => true,
        }
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
            || {},
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
        dispatch_ready: impl FnOnce() + Send + 'static,
        stream_established: impl FnOnce() + Send + 'static,
    ) -> Result<ResponseStream, ApiError> {
        let (tx_event, rx_event) =
            mpsc::channel::<std::result::Result<ResponseEvent, ApiError>>(1600);
        let stream = Arc::clone(&self.stream);
        let idle_timeout = self.idle_timeout;
        let metadata = self.metadata.clone();
        let upstream_request_id = metadata.upstream_request_id().map(str::to_string);
        let telemetry = self.telemetry.clone();
        let request_text = serialize_websocket_request(request)?;
        let (tx_send_complete, rx_send_complete) = oneshot::channel();
        queue_started();

        let current_span = Span::current();
        tokio::spawn(
            #[expect(
                clippy::await_holding_invalid_type,
                reason = "the guard serializes exclusive use of the websocket stream for the lifetime of the response stream"
            )]
            async move {
                let mut guard = stream.lock().await;
                let result = {
                    let Some(ws_stream) = guard.as_mut() else {
                        let _ = tx_send_complete.send(());
                        let _ = tx_event
                            .send(Err(ApiError::Stream(
                                "websocket connection is closed".to_string(),
                            )))
                            .await;
                        return;
                    };

                    dispatch_ready();
                    let send_result = send_websocket_request(
                        ws_stream,
                        request_text,
                        idle_timeout,
                        telemetry.as_ref(),
                        connection_reused,
                    )
                    .await;
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
                                return;
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
                    let failed_stream = guard.take();
                    drop(guard);
                    drop(failed_stream);
                    let _ = tx_event.send(Err(err)).await;
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
    /// WebSocket close code returned by the server.
    pub code: String,
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
        self.auth.add_auth_headers(&mut headers);

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
        self.auth.add_auth_headers(&mut headers);

        let connected = connect_websocket(
            ws_url.clone(),
            headers,
            http_client_factory,
            /*turn_state*/ None,
        )
        .await?;
        let mut stream = connected.stream;
        let immediate_close = tokio::time::timeout(immediate_close_timeout, stream.next())
            .await
            .ok()
            .flatten()
            .transpose()
            .map_err(|err| {
                ApiError::Stream(format!("failed to read websocket probe event: {err}"))
            })?
            .and_then(immediate_close_from_message);

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
    frame.map(close_frame_to_probe)
}

fn close_frame_to_probe(frame: CloseFrame) -> ResponsesWebsocketClose {
    ResponsesWebsocketClose {
        code: frame.code.to_string(),
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
    info!("connecting to websocket: {url}");

    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|err| ApiError::Stream(format!("failed to build websocket request: {err}")))?;
    request.headers_mut().extend(headers);

    let connector = WebSocketConnector::new(http_client_factory)
        .map_err(|err| ApiError::Stream(format!("failed to configure websocket TLS: {err}")))?
        .with_tcp_nodelay();
    let response = connector.connect(request, websocket_config()).await;

    let (stream, response) = match response {
        Ok((stream, response)) => {
            info!(
                "successfully connected to websocket: {url}, headers: {:?}",
                response.headers()
            );
            (stream, response)
        }
        Err(err) => {
            error!("failed to connect to websocket: {err}, url: {url}");
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
            let body = response
                .body()
                .as_ref()
                .and_then(|bytes| String::from_utf8(bytes.clone()).ok());
            ApiError::Transport(TransportError::Http {
                status,
                url: Some(url.to_string()),
                headers: Some(headers),
                body,
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
            delay: None,
        });
    }

    let status = StatusCode::from_u16(status?).ok()?;
    if status.is_success() {
        return None;
    }

    Some(ApiError::Transport(TransportError::Http {
        status,
        url: None,
        headers: headers.as_ref().map(json_headers_to_http_headers),
        body: Some(original_payload),
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
        let response = tokio::time::timeout(idle_timeout, ws_stream.next())
            .await
            .map_err(|_| ApiError::Stream("idle timeout waiting for websocket".into()));
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
                if let Some(wrapped_error) = parse_wrapped_websocket_error_event(&text)
                    && let Some(error) =
                        map_wrapped_websocket_error_event(wrapped_error, text.to_string())
                {
                    return Err(error);
                }

                let events = match interpreter.process_payload(&text) {
                    Ok(events) => events,
                    Err(ResponsesEventError::Parse(error)) => {
                        debug!("failed to parse websocket event: {error}, data: {text}");
                        continue;
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
            Message::Close(_) => {
                return Err(ApiError::Stream(
                    "websocket closed by server before response.completed".into(),
                ));
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
    request_text: String,
    idle_timeout: Duration,
    telemetry: Option<&Arc<dyn WebsocketTelemetry>>,
    connection_reused: bool,
) -> Result<(), ApiError> {
    let request_start = Instant::now();
    let result = tokio::time::timeout(
        idle_timeout,
        ws_stream.send(Message::Text(request_text.into())),
    )
    .await
    .map_err(|_| ApiError::Stream("idle timeout sending websocket request".into()))
    .and_then(|result| {
        result.map_err(|err| ApiError::Stream(format!("failed to send websocket request: {err}")))
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

fn serialize_websocket_request(request: &ResponsesWsRequest) -> Result<String, ApiError> {
    serde_json::to_string(request)
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
        let (_tx_message, rx_message) = mpsc::unbounded_channel();
        let (tx_done, rx_done) = oneshot::channel();
        let pump_task = tokio::spawn(async move {
            let _rx_command = rx_command;
            let _ = rx_done.await;
        });
        let connection = ResponsesWebsocketConnection::new(
            WsStream {
                tx_command,
                rx_message,
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
        let (tx_message, rx_message) = mpsc::unbounded_channel();
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

        let stream = connection
            .stream_request_with_dispatch_ready(
                &request,
                false,
                None,
                move || queue_events.lock().unwrap().push("queue"),
                move || dispatch_events.lock().unwrap().push("dispatch"),
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

        let previous_payload = serde_json::to_value(&request).expect("serialize previous payload");
        let request_text =
            serialize_websocket_request(&request).expect("serialize websocket request");
        let wire_payload =
            serde_json::from_str::<Value>(&request_text).expect("parse websocket request");

        assert_eq!(wire_payload, previous_payload);
    }

    #[test]
    fn websocket_config_enables_permessage_deflate() {
        let config = websocket_config();
        assert!(config.extensions.permessage_deflate.is_some());
    }

    #[test]
    fn parse_wrapped_websocket_error_event_maps_to_transport_http() {
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
                "x-codex-primary-window-minutes": 15
            }
        })
        .to_string();

        let wrapped_error = parse_wrapped_websocket_error_event(&payload)
            .expect("expected websocket error payload to be parsed");
        let api_error = map_wrapped_websocket_error_event(wrapped_error, payload)
            .expect("expected websocket error payload to map to ApiError");

        let ApiError::Transport(TransportError::Http {
            status,
            headers,
            body,
            ..
        }) = api_error
        else {
            panic!("expected ApiError::Transport(Http)");
        };

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
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
