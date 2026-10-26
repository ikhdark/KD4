use crate::auth::SharedAuthProvider;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::common::ResponsesWsRequest;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::response_event_channel::ResponseEventSender;
use crate::response_event_channel::response_event_channel;
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
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::Utf8Bytes;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
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
    response_history: crate::responses_stream::ConnectionResponseHistory,
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
    capacity_released: Arc<Notify>,
    max_queued_bytes: usize,
}

struct WsIngressReceiver {
    rx: mpsc::Receiver<StagedWsMessage>,
    queued_bytes: Arc<AtomicUsize>,
    capacity_released: Arc<Notify>,
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
    let capacity_released = Arc::new(Notify::new());
    (
        WsIngressSender {
            tx,
            queued_bytes: Arc::clone(&queued_bytes),
            capacity_released: Arc::clone(&capacity_released),
            max_queued_bytes,
        },
        WsIngressReceiver {
            rx,
            queued_bytes,
            capacity_released,
        },
    )
}

impl WsIngressSender {
    async fn send(&self, message: &Message) -> Result<(), WsIngressSendError> {
        let payload_bytes = message.len();
        if payload_bytes > self.max_queued_bytes {
            return Err(WsIngressSendError::Full);
        }
        let permit = self
            .tx
            .reserve()
            .await
            .map_err(|_| WsIngressSendError::Closed)?;
        loop {
            // Register before checking the budget to avoid a lost wakeup.
            let released = self.capacity_released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if self
                .queued_bytes
                .try_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                    queued
                        .checked_add(payload_bytes)
                        .filter(|next| *next <= self.max_queued_bytes)
                })
                .is_ok()
            {
                // No await after charging the budget: cancellation cannot leak it.
                permit.send(StagedWsMessage {
                    message: message.clone(),
                    payload_bytes,
                });
                return Ok(());
            }
            tokio::select! {
                _ = self.tx.closed() => return Err(WsIngressSendError::Closed),
                _ = released => {}
            }
        }
    }
}

impl WsIngressReceiver {
    async fn recv(&mut self) -> Option<Message> {
        let staged = self.rx.recv().await?;
        self.queued_bytes
            .fetch_sub(staged.payload_bytes, Ordering::AcqRel);
        self.capacity_released.notify_waiters();
        Some(staged.message)
    }

    fn try_recv(&mut self) -> Option<Message> {
        let staged = self.rx.try_recv().ok()?;
        self.queued_bytes
            .fetch_sub(staged.payload_bytes, Ordering::AcqRel);
        self.capacity_released.notify_waiters();
        Some(staged.message)
    }
}

enum WsCommand {
    Send {
        message: Message,
        dispatch_guard: Option<Arc<dyn codex_http_client::RequestDispatchGuard>>,
        tx_result: oneshot::Sender<Result<(), WsSendError>>,
    },
}

#[derive(Debug)]
enum WsSendError {
    Admission(String),
    Socket(WsError),
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
            let mut pending_message = None;
            loop {
                tokio::select! {
                    command = rx_command.recv() => {
                        let Some(command) = command else {
                            break;
                        };
                        match command {
                            WsCommand::Send { message, dispatch_guard, tx_result } => {
                                // A cancelled queued request must never be sent later.
                                if tx_result.is_closed() {
                                    continue;
                                }
                                let result = codex_http_client::with_dispatch_guard(dispatch_guard,
                                    codex_http_client::admit_dispatch(inner.send(message))).await
                                    .map_err(WsSendError::Admission)
                                    .and_then(|result| result.map_err(WsSendError::Socket));
                                let should_break = matches!(&result, Err(WsSendError::Socket(_)));
                                let _ = tx_result.send(result);
                                if should_break {
                                    break;
                                }
                            }
                        }
                    }
                    // Pause reads at the ingress bounds, but keep dispatch commands
                    // runnable: a reused connection sends before draining ingress.
                    result = async {
                        match pending_message.as_ref() {
                            Some(message) => tx_message.send(message).await,
                            None => std::future::pending().await,
                        }
                    }, if pending_message.is_some() => {
                        match result {
                            Ok(()) => {
                                if matches!(pending_message.take(), Some(Message::Close(_))) {
                                    break;
                                }
                            }
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
                    }
                    message = inner.next(), if pending_message.is_none() => {
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
                                pending_message = Some(message);
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
            response_history: Default::default(),
            pump_task,
        }
    }

    async fn send(&self, message: Message, idle_timeout: Duration) -> Result<(), ApiError> {
        let dispatch_guard = codex_http_client::bind_dispatch_guard(None);
        let deadline = Instant::now() + idle_timeout;
        // Reserve before transferring the non-idempotent command. Failure here
        // proves no dispatch; after transfer, even a lost acknowledgement is unknown.
        let permit = tokio::time::timeout_at(deadline, self.tx_command.reserve())
            .await
            .map_err(|_| ApiError::Retryable {
                message: "timeout waiting for websocket send queue before dispatch".into(),
                delay: None,
            })?
            .map_err(|_| ApiError::Retryable {
                message: "websocket send queue closed before dispatch".into(),
                delay: None,
            })?;
        let (tx_result, rx_result) = oneshot::channel();
        permit.send(WsCommand::Send { message, dispatch_guard, tx_result });
        let reason = match tokio::time::timeout_at(deadline, rx_result).await {
            Ok(Ok(Ok(()))) => return Ok(()),
            Ok(Ok(Err(WsSendError::Admission(error)))) => return Err(ApiError::Transport(TransportError::Build(error))),
            Ok(Ok(Err(WsSendError::Socket(error)))) => format!("failed to send websocket request: {error}"),
            Ok(Err(_)) => "websocket send acknowledgement was lost".into(),
            Err(_) => format!(
                "idle timeout sending websocket request after {}ms",
                idle_timeout.as_millis(),
            ),
        };
        Err(ApiError::ProviderFailure {
            code: Some("dispatch_outcome_unknown".into()),
            message: format!(
                "Model request outcome is unknown after dispatch ({reason}); automatic replay was stopped to avoid duplicate generation."
            ),
        })
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
        self.pending_failure.is_some()
            || self.pump_task.is_finished()
            || self.response_history.must_retire()
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
    dispatch_auth: Option<SharedAuthProvider>,
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
            dispatch_auth: None,
            retired: Arc::new(AtomicBool::new(false)),
            stream: Arc::new(Mutex::new(Some(stream))),
            idle_timeout,
            metadata,
            telemetry,
        }
    }

    pub fn logical_transport_id(&self) -> Option<&str> {
        self.telemetry.as_ref()?.logical_transport_id()
    }

    pub async fn is_closed(&self) -> bool {
        self.retired.load(Ordering::Acquire) || self.pump_handle.is_finished()
    }

    /// Handoff must exclude an active response that may still retire the socket
    /// when its consumer is dropped. Inspect the existing exclusive owner without
    /// waiting; connection health remains a separate check at reuse time.
    pub fn has_active_response(&self) -> bool {
        self.stream.try_lock().is_err()
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

    pub async fn stream_request_with_dispatch_ready(
        &self,
        request: &ResponsesWsRequest,
        connection_reused: bool,
        turn_state: Option<Arc<OnceLock<String>>>,
        queue_started: impl FnOnce(),
        dispatch_ready: impl FnOnce(Bytes) + Send + 'static,
        stream_established: impl FnOnce() + Send + 'static,
    ) -> Result<ResponseStream, ApiError> {
        self.stream_request_with_response_idle_timeout(
            request,
            connection_reused,
            turn_state,
            /*response_idle_timeout*/ None,
            queue_started,
            dispatch_ready,
            stream_established,
        )
        .await
    }

    /// Like [`Self::stream_request_with_dispatch_ready`], but bounds silence
    /// while waiting for response progress by `response_idle_timeout` when set.
    /// Queueing and sending keep the connection's idle timeout.
    #[allow(clippy::too_many_arguments)]
    #[instrument(
        name = "responses_websocket.stream_request",
        level = "info",
        skip_all,
        fields(transport = "responses_websocket", api.path = "responses")
    )]
    pub async fn stream_request_with_response_idle_timeout(
        &self,
        request: &ResponsesWsRequest,
        connection_reused: bool,
        turn_state: Option<Arc<OnceLock<String>>>,
        response_idle_timeout: Option<Duration>,
        queue_started: impl FnOnce(),
        dispatch_ready: impl FnOnce(Bytes) + Send + 'static,
        stream_established: impl FnOnce() + Send + 'static,
    ) -> Result<ResponseStream, ApiError> {
        self.stream_request_with_transport_attempt(
            request,
            connection_reused,
            turn_state,
            response_idle_timeout,
            queue_started,
            dispatch_ready,
            stream_established,
            None,
        )
        .await
    }

    /// Per-request observer, invoked only when the selected send future is
    /// polled. Queue readiness and successful send retain their own callbacks.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_request_with_transport_attempt(
        &self,
        request: &ResponsesWsRequest,
        connection_reused: bool,
        turn_state: Option<Arc<OnceLock<String>>>,
        response_idle_timeout: Option<Duration>,
        queue_started: impl FnOnce(),
        dispatch_ready: impl FnOnce(Bytes) + Send + 'static,
        stream_established: impl FnOnce() + Send + 'static,
        transport_attempt: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<ResponseStream, ApiError> {
        self.stream_request_with_observers(
            request,
            connection_reused,
            turn_state,
            response_idle_timeout,
            queue_started,
            dispatch_ready,
            stream_established,
            transport_attempt,
            None,
        )
        .await
    }

    /// Override the connection observer for this physical request only.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_request_with_observers(
        &self,
        request: &ResponsesWsRequest,
        connection_reused: bool,
        turn_state: Option<Arc<OnceLock<String>>>,
        response_idle_timeout: Option<Duration>,
        queue_started: impl FnOnce(),
        dispatch_ready: impl FnOnce(Bytes) + Send + 'static,
        stream_established: impl FnOnce() + Send + 'static,
        transport_attempt: Option<Arc<dyn Fn() + Send + Sync>>,
        request_telemetry: Option<Arc<dyn WebsocketTelemetry>>,
    ) -> Result<ResponseStream, ApiError> {
        let (tx_event, rx_event) = response_event_channel::<ApiError>(1600);
        let stream = Arc::clone(&self.stream);
        let dispatch_auth = self.dispatch_auth.clone();
        let dispatch_guard = codex_http_client::bind_dispatch_guard(
            dispatch_auth
                .as_ref()
                .and_then(|auth| auth.dispatch_guard()),
        );
        let retired = Arc::clone(&self.retired);
        let idle_timeout = self.idle_timeout;
        let response_idle_timeout = response_idle_timeout.unwrap_or(idle_timeout);
        let metadata = self.metadata.clone();
        let upstream_request_id = metadata.upstream_request_id().map(str::to_string);
        let telemetry = request_telemetry.or_else(|| self.telemetry.clone());
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
                    guard = tokio::time::timeout(idle_timeout, stream.lock()) => {
                        match guard {
                            Ok(guard) => guard,
                            Err(_) => {
                                // Nothing was sent: do not retire the active connection or
                                // turn local contention into a provider replay/fallback.
                                let _ = tx_send_complete.send(());
                                let _ = tx_event.send(Err(ApiError::Transport(
                                    TransportError::PreDispatch(
                                        "timeout waiting for websocket dispatch queue".to_string(),
                                    ),
                                ))).await;
                                return;
                            }
                        }
                    },
                };
                // Abandon queued work without invalidating an unused connection.
                if tx_event.is_closed() {
                    return;
                }
                // Queue admission may outlive logout/account replacement. Check
                // after the exclusive socket wait, not when setup was prepared.
                if let Some(auth) = dispatch_auth
                    && let Err(err) = auth.try_add_auth_headers(&mut HeaderMap::new())
                {
                    let _ = tx_send_complete.send(());
                    let _ = tx_event
                        .send(Err(ApiError::Transport(TransportError::from(err))))
                        .await;
                    return;
                }
                let result = 'response: {
                    let Some(ws_stream) = guard.as_mut() else {
                        let _ = tx_send_complete.send(());
                        let _ = tx_event
                            .send(Err(ApiError::Transport(TransportError::PreDispatch(
                                "websocket connection retired before dispatch".to_string(),
                            ))))
                            .await;
                        return;
                    };

                    dispatch_ready(encoded_request);
                    let send_result = tokio::select! {
                        biased;
                        _ = tx_event.closed() => Err(ApiError::Stream(
                            "response event consumer dropped".to_string(),
                        )),
                        result = codex_http_client::with_dispatch_guard(dispatch_guard, async {
                            codex_http_client::admit_dispatch(send_websocket_request(
                            ws_stream,
                            request_text,
                            idle_timeout,
                            telemetry.as_ref(),
                            connection_reused,
                            transport_attempt.as_deref(),
                            )).await.map_err(|error| ApiError::Transport(TransportError::Build(error)))?
                        }) => result,
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
                            response_idle_timeout,
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
            .map_err(|err| {
                ApiError::Transport(TransportError::Build(format!(
                    "failed to build websocket URL: {err}"
                )))
            })?;

        let mut headers =
            merge_request_headers(&self.provider.headers, extra_headers, default_headers);
        self.auth
            .try_add_auth_headers(&mut headers)
            .map_err(TransportError::from)?;

        let connected = codex_http_client::with_dispatch_guard(
            self.auth.dispatch_guard(),
            connect_websocket(ws_url, headers, http_client_factory, turn_state.clone()),
        )
        .await?;
        let mut connection = ResponsesWebsocketConnection::new(
            connected.stream,
            self.provider.stream_idle_timeout,
            connected.metadata,
            telemetry,
        );
        connection.dispatch_auth = Some(self.auth.clone());
        Ok(connection)
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
            .map_err(|err| {
                ApiError::Transport(TransportError::Build(format!(
                    "failed to build websocket URL: {err}"
                )))
            })?;

        let mut headers =
            merge_request_headers(&self.provider.headers, extra_headers, default_headers);
        self.auth
            .try_add_auth_headers(&mut headers)
            .map_err(TransportError::from)?;

        let connected = codex_http_client::with_dispatch_guard(
            self.auth.dispatch_guard(),
            connect_websocket(
                ws_url.clone(),
                headers,
                http_client_factory,
                /*turn_state*/ None,
            ),
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

    let mut request = url.as_str().into_client_request().map_err(|err| {
        ApiError::Transport(TransportError::Build(format!(
            "failed to build websocket request: {err}"
        )))
    })?;
    request.headers_mut().extend(headers);

    let http_client_factory = http_client_factory.clone();
    let connector =
        tokio::task::spawn_blocking(move || WebSocketConnector::new(&http_client_factory))
            .await
            .map_err(|err| {
                ApiError::Stream(format!("websocket TLS configuration task failed: {err}"))
            })?
            .map_err(|err| {
                ApiError::Transport(TransportError::Build(format!(
                    "failed to configure websocket TLS: {err}"
                )))
            })?
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
        WsError::Tls(error) if codex_http_client::is_permanent_connection_error(&error) => {
            ApiError::Transport(TransportError::Build(format!(
                "permanent websocket TLS configuration failure: {error}"
            )))
        }
        WsError::Url(err) => ApiError::Transport(TransportError::Build(format!(
            "invalid websocket configuration: {err}"
        ))),
        // Lost dispatch authority before any byte was sent. Match the HTTP and
        // WebSocket send paths: a request-build failure, not a transport fault
        // that would demote WebSockets for the session.
        WsError::Io(err)
            if err
                .get_ref()
                .is_some_and(
                    <dyn std::error::Error + Send + Sync>::is::<codex_websocket_client::DispatchRejected>,
                ) =>
        {
            ApiError::Transport(TransportError::Build(err.to_string()))
        }
        WsError::Io(err) if codex_http_client::is_permanent_connection_error(&err) => {
            ApiError::Transport(TransportError::Build(format!(
                "permanent websocket configuration failure: {err}"
            )))
        }
        WsError::Io(err) => ApiError::Transport(TransportError::Network(err.to_string())),
        other => ApiError::Transport(TransportError::Network(other.to_string())),
    }
}

#[derive(Debug, Deserialize)]
struct WrappedWebsocketError {
    code: Option<String>,
    message: Option<String>,
    headers: Option<Value>,
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
    // Nested error headers describe this rejection, not the surrounding frame.
    // Keep top-level headers as a compatibility fallback.
    let mut headers = headers.as_ref().map(json_headers_to_http_headers);
    let mut retry_after = headers
        .as_ref()
        .and_then(codex_http_client::RetryAfter::from_headers);
    if let Some(nested) = error.as_ref()
        .and_then(|error| error.headers.as_ref())
        .and_then(Value::as_object)
    {
        let nested = json_headers_to_http_headers(nested);
        retry_after = codex_http_client::RetryAfter::from_headers(&nested).or(retry_after);
        headers.get_or_insert_with(HeaderMap::new)
            .extend(nested);
    }

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
        return Some(ApiError::WebsocketRejected {
            reason: if code == PREVIOUS_RESPONSE_NOT_FOUND_CODE {
                codex_protocol::error::WebsocketRecoveryReason::PreviousResponseNotFound
            } else {
                codex_protocol::error::WebsocketRecoveryReason::ConnectionLimitReached
            },
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
    tx_event: ResponseEventSender<ApiError>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn WebsocketTelemetry>>,
    metadata: ResponsesStreamMetadata,
    turn_state: Option<Arc<OnceLock<String>>>,
) -> Result<(), ApiError> {
    let history = std::mem::take(&mut ws_stream.response_history);
    let mut interpreter =
        ResponsesEventInterpreter::new(&metadata, turn_state).with_connection_history(history);
    let mut progress_deadline = tokio::time::Instant::now() + idle_timeout;
    loop {
        if tokio::time::Instant::now() >= progress_deadline {
            return Err(ApiError::Stream(
                "model progress timeout waiting for websocket".into(),
            ));
        }
        let poll_start = Instant::now();
        let response = tokio::select! {
            biased;
            _ = tx_event.closed() => return Err(ApiError::Stream(
                "response event consumer dropped".to_string(),
            )),
            response = tokio::time::timeout_at(progress_deadline, ws_stream.next()) => response,
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
                        return Err(ApiError::invalid_response(
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
                let mut completed = false;
                for event in events {
                    if let Some(telemetry) = telemetry.as_ref() {
                        telemetry.on_response_event();
                    }
                    let advances = event.advances_model_response();
                    let is_completed = matches!(event, ResponseEvent::Completed { .. });
                    let delivery_start = Instant::now();
                    tx_event.send(Ok(event)).await?;
                    if advances {
                        progress_deadline = tokio::time::Instant::now() + idle_timeout;
                    } else {
                        progress_deadline += delivery_start.elapsed();
                    }
                    completed |= is_completed;
                }
                if completed {
                    ws_stream.response_history = interpreter.into_connection_history();
                    return Ok(());
                }
            }
            Message::Binary(_) => {
                return Err(ApiError::invalid_response(
                    "unexpected binary websocket event",
                ));
            }
            Message::Close(frame) => {
                let mut message =
                    "websocket closed by server before response.completed".to_string();
                let mut reconnectable = false;
                if let Some(frame) = frame {
                    message.push_str(&format!(
                        " (code {}, reason: {:?})",
                        frame.code,
                        frame.reason.as_str()
                    ));
                    reconnectable = close_code_invites_reconnect(frame.code);
                }
                return Err(if reconnectable {
                    ApiError::reconnectable_close(message)
                } else {
                    ApiError::Stream(message)
                });
            }
            Message::Frame(_) => {}
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }
}

/// Whether a mid-response close code means "connect again", not "do not".
///
/// The server closes with these codes when it sheds load or restarts a node,
/// which is unrelated to this request: the same request on a new socket
/// normally completes. Every other code — a policy rejection, a protocol or
/// payload error, an authentication failure — describes the request or the
/// client, so reconnecting would only repeat the rejection.
fn close_code_invites_reconnect(code: CloseCode) -> bool {
    matches!(
        code,
        CloseCode::Normal | CloseCode::Away | CloseCode::Restart | CloseCode::Again
    )
}

fn websocket_read_error(err: WsError) -> ApiError {
    let message = match err {
        WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => {
            "websocket closed before response.completed".to_string()
        }
        WsError::Protocol(_) => return ApiError::invalid_response("invalid websocket protocol frame"),
        WsError::Capacity(_) => return ApiError::invalid_response("websocket frame exceeds capacity"),
        WsError::Utf8(_) => return ApiError::invalid_response("invalid websocket UTF-8 message"),
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
    transport_attempt: Option<&(dyn Fn() + Send + Sync)>,
) -> Result<(), ApiError> {
    let request_start = Instant::now();
    if let Some(observer) = transport_attempt {
        observer();
    }
    let result = ws_stream
        .send(Message::Text(request_text), idle_timeout)
        .await;

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
