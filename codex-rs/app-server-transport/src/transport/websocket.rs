use super::CHANNEL_CAPACITY;
use super::ConnectionOrigin;
use super::TransportEvent;
use super::auth::WebsocketAuthPolicy;
use super::auth::authorize_upgrade;
use super::auth::is_unauthenticated_non_loopback_listener;
use super::forward_incoming_message;
use super::next_connection_id;
use super::serialize_outgoing_message;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::QueuedOutgoingMessage;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::extract::State;
use axum::extract::ws::Message as AxumWebSocketMessage;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header::ORIGIN;
use axum::middleware;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::any;
use axum::routing::get;
use futures::SinkExt;
use futures::StreamExt;
use owo_colors::OwoColorize;
use owo_colors::Stream;
use owo_colors::Style;
use std::io::Result as IoResult;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as TungsteniteWebSocketMessage;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tokio_util::task::TaskTracker;
use tracing::error;
use tracing::info;
use tracing::warn;

/// WebSocket clients can briefly lag behind normal turn output bursts while the
/// writer task is healthy, so give them more headroom than internal channels.
// Eight internal-channel bursts; saturation disconnects only the slow client.
const WEBSOCKET_OUTBOUND_CHANNEL_CAPACITY: usize = 8 * CHANNEL_CAPACITY;
const WEBSOCKET_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
const _: () = assert!(WEBSOCKET_OUTBOUND_CHANNEL_CAPACITY > CHANNEL_CAPACITY);

fn colorize(text: &str, style: Style) -> String {
    text.if_supports_color(Stream::Stderr, |value| value.style(style))
        .to_string()
}

#[allow(clippy::print_stderr)]
fn print_websocket_startup_banner(addr: SocketAddr) {
    let title = colorize("codex app-server (WebSockets)", Style::new().bold().cyan());
    let listening_label = colorize("listening on:", Style::new().dimmed());
    let listen_url = colorize(&format!("ws://{addr}"), Style::new().green());
    let ready_label = colorize("readyz:", Style::new().dimmed());
    let ready_url = colorize(&format!("http://{addr}/readyz"), Style::new().green());
    let health_label = colorize("healthz:", Style::new().dimmed());
    let health_url = colorize(&format!("http://{addr}/healthz"), Style::new().green());
    let note_label = colorize("note:", Style::new().dimmed());
    eprintln!("{title}");
    eprintln!("  {listening_label} {listen_url}");
    eprintln!("  {ready_label} {ready_url}");
    eprintln!("  {health_label} {health_url}");
    if addr.ip().is_loopback() {
        eprintln!(
            "  {note_label} binds localhost only (use SSH port-forwarding for remote access)"
        );
    } else {
        eprintln!("  {note_label} websocket auth is required for non-localhost listeners");
    }
}

#[derive(Clone)]
struct WebSocketListenerState {
    transport_event_tx: mpsc::Sender<TransportEvent>,
    auth_policy: Arc<WebsocketAuthPolicy>,
    shutdown_token: CancellationToken,
    connection_tasks: TaskTracker,
}

async fn readiness_handler(State(state): State<WebSocketListenerState>) -> StatusCode {
    if state.transport_event_tx.is_closed() || state.shutdown_token.is_cancelled() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

async fn health_check_handler() -> StatusCode {
    StatusCode::OK
}

async fn reject_requests_with_origin_header(
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if request.headers().contains_key(ORIGIN) {
        warn!(
            method = %request.method(),
            uri = %request.uri(),
            "rejecting websocket listener request with Origin header"
        );
        Err(StatusCode::FORBIDDEN)
    } else {
        Ok(next.run(request).await)
    }
}

async fn websocket_upgrade_handler(
    websocket: WebSocketUpgrade,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    State(state): State<WebSocketListenerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(err) = authorize_upgrade(&headers, state.auth_policy.as_ref()) {
        warn!(
            %peer_addr,
            message = err.message(),
            "rejecting websocket client during upgrade"
        );
        return (err.status_code(), err.message()).into_response();
    }
    info!(%peer_addr, "websocket client connected");
    // Track from admission, including upgrades that have not started their callback yet.
    let connection_guard = state.connection_tasks.token();
    websocket
        .on_upgrade(move |stream| async move {
            let _connection_guard = connection_guard;
            let (websocket_writer, websocket_reader) = stream.split();
            run_websocket_connection(
                websocket_writer,
                websocket_reader,
                state.transport_event_tx,
                state.shutdown_token,
            )
            .await;
        })
        .into_response()
}

pub async fn start_websocket_acceptor(
    bind_address: SocketAddr,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    shutdown_token: CancellationToken,
    auth_policy: WebsocketAuthPolicy,
) -> IoResult<JoinHandle<()>> {
    validate_websocket_listener(bind_address, &auth_policy)?;
    let listener = TcpListener::bind(bind_address).await?;
    start_websocket_acceptor_with_listener(
        listener,
        transport_event_tx,
        shutdown_token,
        auth_policy,
    )
}

fn start_websocket_acceptor_with_listener(
    listener: TcpListener,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    shutdown_token: CancellationToken,
    auth_policy: WebsocketAuthPolicy,
) -> IoResult<JoinHandle<()>> {
    let local_addr = listener.local_addr()?;
    print_websocket_startup_banner(local_addr);
    info!("app-server websocket listening on ws://{local_addr}");

    let connection_tasks = TaskTracker::new();
    let router = Router::new()
        .route("/readyz", get(readiness_handler))
        .route("/healthz", get(health_check_handler))
        .fallback(any(websocket_upgrade_handler))
        .layer(middleware::from_fn(reject_requests_with_origin_header))
        .with_state(WebSocketListenerState {
            transport_event_tx,
            auth_policy: Arc::new(auth_policy),
            shutdown_token: shutdown_token.clone(),
            connection_tasks: connection_tasks.clone(),
        });
    let listener_shutdown = shutdown_token.clone();
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        listener_shutdown.cancelled().await;
    });
    Ok(tokio::spawn(async move {
        if let Err(err) = server.await {
            error!("websocket acceptor failed: {err}");
        }
        shutdown_token.cancel();
        connection_tasks.close();
        connection_tasks.wait().await;
        info!("websocket acceptor shutting down");
    }))
}

pub fn validate_websocket_listener(
    bind_address: SocketAddr,
    auth_policy: &WebsocketAuthPolicy,
) -> IoResult<()> {
    if is_unauthenticated_non_loopback_listener(bind_address, auth_policy) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to start non-loopback websocket listener {bind_address} without auth; configure `--ws-auth capability-token` or `--ws-auth signed-bearer-token`"
            ),
        ));
    }
    Ok(())
}

pub(crate) async fn run_websocket_connection<M, SinkError, StreamError>(
    websocket_writer: impl futures::sink::Sink<M, Error = SinkError> + Send + 'static,
    websocket_reader: impl futures::stream::Stream<Item = Result<M, StreamError>> + Send + 'static,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    shutdown_token: CancellationToken,
) where
    M: AppServerWebSocketMessage + Send + 'static,
    SinkError: Send + 'static,
    StreamError: std::fmt::Display + Send + 'static,
{
    let connection_id = next_connection_id();
    let (writer_tx, writer_rx) =
        mpsc::channel::<QueuedOutgoingMessage>(WEBSOCKET_OUTBOUND_CHANNEL_CAPACITY);
    let writer_tx_for_reader = writer_tx.clone();
    let disconnect_token = shutdown_token.child_token();
    let registered = tokio::select! {
        biased;
        _ = disconnect_token.cancelled() => return,
        result = transport_event_tx
        .send(TransportEvent::ConnectionOpened {
            connection_id,
            origin: ConnectionOrigin::WebSocket,
            writer: writer_tx,
            disconnect_sender: Some(disconnect_token.clone()),
        })
        => result,
    };
    if registered.is_err() {
        return;
    }

    let (writer_control_tx, writer_control_rx) =
        mpsc::channel::<WebSocketControl>(CHANNEL_CAPACITY);
    let mut outbound_task = AbortOnDropHandle::new(tokio::spawn(run_websocket_outbound_loop(
        websocket_writer,
        writer_rx,
        writer_control_rx,
        disconnect_token.clone(),
    )));
    let mut inbound_task = AbortOnDropHandle::new(tokio::spawn(run_websocket_inbound_loop(
        websocket_reader,
        transport_event_tx.clone(),
        writer_tx_for_reader,
        writer_control_tx,
        connection_id,
        disconnect_token.clone(),
    )));

    tokio::select! {
        _ = disconnect_token.cancelled() => {
            outbound_task.abort();
            inbound_task.abort();
            let _ = outbound_task.await;
            let _ = inbound_task.await;
        }
        _ = &mut outbound_task => {
            disconnect_token.cancel();
            inbound_task.abort();
            let _ = inbound_task.await;
        }
        result = &mut inbound_task => {
            // Tungstenite queues the peer's close reply while reading. Let the writer
            // flush it, but never let a stalled peer delay cancellation or shutdown.
            let outbound_finished = if matches!(result, Ok(true)) {
                tokio::select! {
                    _ = disconnect_token.cancelled() => false,
                    result = timeout(WEBSOCKET_CLOSE_TIMEOUT, &mut outbound_task) => result.is_ok(),
                }
            } else {
                false
            };
            disconnect_token.cancel();
            if !outbound_finished {
                outbound_task.abort();
                let _ = outbound_task.await;
            }
        }
    }

    let _ = transport_event_tx
        .send(TransportEvent::ConnectionClosed { connection_id })
        .await;
}

pub(crate) enum IncomingWebSocketMessage<'a> {
    Text(&'a str),
    Binary,
    Ping,
    Pong,
    Close,
}

/// Converts concrete WebSocket message types into the small message surface the
/// app-server transport needs, and constructs the only outbound frames it
/// sends directly.
pub(crate) trait AppServerWebSocketMessage: Sized {
    fn text(text: String) -> Self;
    fn incoming(&self) -> Option<IncomingWebSocketMessage<'_>>;
}

impl AppServerWebSocketMessage for AxumWebSocketMessage {
    fn text(text: String) -> Self {
        Self::Text(text.into())
    }

    fn incoming(&self) -> Option<IncomingWebSocketMessage<'_>> {
        Some(match self {
            Self::Text(text) => IncomingWebSocketMessage::Text(text.as_str()),
            Self::Binary(_) => IncomingWebSocketMessage::Binary,
            Self::Ping(_) => IncomingWebSocketMessage::Ping,
            Self::Pong(_) => IncomingWebSocketMessage::Pong,
            Self::Close(_) => IncomingWebSocketMessage::Close,
        })
    }
}

impl AppServerWebSocketMessage for TungsteniteWebSocketMessage {
    fn text(text: String) -> Self {
        Self::Text(text.into())
    }

    fn incoming(&self) -> Option<IncomingWebSocketMessage<'_>> {
        Some(match self {
            Self::Text(text) => IncomingWebSocketMessage::Text(text.as_str()),
            Self::Binary(_) => IncomingWebSocketMessage::Binary,
            Self::Ping(_) => IncomingWebSocketMessage::Ping,
            Self::Pong(_) => IncomingWebSocketMessage::Pong,
            Self::Close(_) => IncomingWebSocketMessage::Close,
            Self::Frame(_) => return None,
        })
    }
}

enum WebSocketControl {
    Flush,
    FlushClose,
}

async fn run_websocket_outbound_loop<M, SinkError>(
    websocket_writer: impl futures::sink::Sink<M, Error = SinkError> + Send + 'static,
    mut writer_rx: mpsc::Receiver<QueuedOutgoingMessage>,
    mut writer_control_rx: mpsc::Receiver<WebSocketControl>,
    disconnect_token: CancellationToken,
) where
    M: AppServerWebSocketMessage + Send + 'static,
    SinkError: Send + 'static,
{
    tokio::pin!(websocket_writer);
    loop {
        tokio::select! {
            biased;
            _ = disconnect_token.cancelled() => {
                break;
            }
            message = writer_control_rx.recv() => {
                let Some(message) = message else {
                    break;
                };
                match message {
                    WebSocketControl::Flush => {
                        if websocket_writer.flush().await.is_err() {
                            break;
                        }
                    }
                    WebSocketControl::FlushClose => {
                        let _ = websocket_writer.flush().await;
                        break;
                    }
                }
            }
            queued_message = writer_rx.recv() => {
                let Some(queued_message) = queued_message else {
                    break;
                };
                let Some(json) = serialize_outgoing_message(queued_message.message) else {
                    continue;
                };
                if websocket_writer.send(M::text(json)).await.is_err() {
                    break;
                }
                if let Some(write_complete_tx) = queued_message.write_complete_tx {
                    let _ = write_complete_tx.send(());
                }
            }
        }
    }
}

async fn run_websocket_inbound_loop<M, StreamError>(
    websocket_reader: impl futures::stream::Stream<Item = Result<M, StreamError>> + Send + 'static,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    writer_tx_for_reader: mpsc::Sender<QueuedOutgoingMessage>,
    writer_control_tx: mpsc::Sender<WebSocketControl>,
    connection_id: ConnectionId,
    disconnect_token: CancellationToken,
) -> bool
where
    M: AppServerWebSocketMessage + Send + 'static,
    StreamError: std::fmt::Display + Send + 'static,
{
    tokio::pin!(websocket_reader);
    loop {
        tokio::select! {
            _ = disconnect_token.cancelled() => {
                break;
            }
            incoming_message = websocket_reader.next() => {
                match incoming_message {
                    Some(Ok(message)) => match message.incoming() {
                        Some(IncomingWebSocketMessage::Text(text))
                            if !forward_incoming_message(
                                &transport_event_tx,
                                &writer_tx_for_reader,
                                connection_id,
                                text,
                            )
                            .await
                        => {
                            break;
                        }
                        Some(IncomingWebSocketMessage::Text(_)) => {}
                        Some(IncomingWebSocketMessage::Ping) => {
                            // Both transports queue an automatic pong while reading.
                            match writer_control_tx.try_send(WebSocketControl::Flush) {
                                Ok(()) => {}
                                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                    // A queued flush already drains the automatic pong.
                                    // Coalesce redundant requests while the writer is busy.
                                }
                            }
                        }
                        Some(IncomingWebSocketMessage::Pong) => {}
                        Some(IncomingWebSocketMessage::Close) => {
                            return writer_control_tx.try_send(WebSocketControl::FlushClose).is_ok();
                        }
                        Some(IncomingWebSocketMessage::Binary) => {
                            warn!("dropping unsupported binary websocket message");
                        }
                        None => {}
                    },
                    None => break,
                    Some(Err(err)) => {
                        warn!("websocket receive error: {err}");
                        break;
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outgoing_message::OutgoingMessage;
    use crate::outgoing_message::OutgoingResponse;
    use codex_app_server_protocol::RequestId;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::task::Context;
    use std::task::Poll;

    #[tokio::test]
    async fn ping_with_full_control_queue_keeps_forwarding_messages() {
        let (events_tx, mut events_rx) = mpsc::channel(1);
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (control_tx, mut control_rx) = mpsc::channel(1);
        assert!(control_tx.try_send(WebSocketControl::Flush).is_ok());
        let frames = futures::stream::iter([
            Ok::<_, std::io::Error>(TungsteniteWebSocketMessage::Ping(Vec::new().into())),
            Ok(TungsteniteWebSocketMessage::Text(
                r#"{"id":17,"result":{"ok":true}}"#.into(),
            )),
        ])
        .chain(futures::stream::pending());
        let disconnect = CancellationToken::new();
        let inbound = run_websocket_inbound_loop(
            frames,
            events_tx,
            writer_tx,
            control_tx,
            ConnectionId(7),
            disconnect.clone(),
        );
        tokio::pin!(inbound);
        assert!(futures::poll!(&mut inbound).is_pending());
        match events_rx.try_recv().unwrap() {
            TransportEvent::IncomingMessage {
                connection_id,
                message,
            } => {
                assert_eq!(connection_id, ConnectionId(7));
                assert_eq!(
                    serde_json::to_value(message).unwrap(),
                    serde_json::json!({"id":17,"result":{"ok":true}})
                );
            }
            event => panic!("expected message after ping: {event:?}"),
        }
        assert!(matches!(control_rx.try_recv(), Ok(WebSocketControl::Flush)));
        assert!(matches!(
            control_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        disconnect.cancel();
        assert!(!timeout(Duration::from_secs(1), inbound).await.unwrap());
        assert!(events_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn ping_with_closed_control_queue_stops_forwarding_messages() {
        let (events_tx, mut events_rx) = mpsc::channel(1);
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (control_tx, control_rx) = mpsc::channel(1);
        drop(control_rx);
        let frames = futures::stream::iter([
            Ok::<_, std::io::Error>(TungsteniteWebSocketMessage::Ping(Vec::new().into())),
            Ok(TungsteniteWebSocketMessage::Text(
                r#"{"id":17,"result":{"ok":true}}"#.into(),
            )),
        ]);
        assert!(
            !run_websocket_inbound_loop(
                frames,
                events_tx,
                writer_tx,
                control_tx,
                ConnectionId(7),
                CancellationToken::new(),
            )
            .await
        );
        assert!(matches!(
            events_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn tcp_listener_shutdown_awaits_established_connection() {
        timeout(Duration::from_secs(3), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (events_tx, mut events_rx) = mpsc::channel(8);
            let shutdown = CancellationToken::new();
            let acceptor = start_websocket_acceptor_with_listener(listener, events_tx, shutdown.clone(), WebsocketAuthPolicy::default()).unwrap();
            let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/rpc")).await.unwrap();
            let (connection_id, writer) = match events_rx.recv().await.unwrap() {
                TransportEvent::ConnectionOpened { connection_id, writer, .. } => (connection_id, writer),
                event => panic!("expected registration: {event:?}"),
            };
            // Exercise the Axum text adapter before shutting down with the peer open.
            client.send(TungsteniteWebSocketMessage::Text(r#"{"id":17,"result":{"ok":true}}"#.into())).await.unwrap();
            match events_rx.recv().await.unwrap() {
                TransportEvent::IncomingMessage { connection_id: id, message } => {
                    assert_eq!(id, connection_id);
                    assert_eq!(serde_json::to_value(message).unwrap(), serde_json::json!({"id":17,"result":{"ok":true}}));
                }
                event => panic!("expected forwarded response: {event:?}"),
            }
            shutdown.cancel();
            acceptor.await.unwrap();
            assert!(writer.is_closed());
            assert!(matches!(events_rx.recv().await, Some(TransportEvent::ConnectionClosed { connection_id: id }) if id == connection_id));
            assert!(events_rx.recv().await.is_none());
            assert!(matches!(client.next().await, None | Some(Err(_))));
        }).await.expect("listener must await connection cleanup");
    }

    #[tokio::test]
    async fn readiness_tracks_processor_availability_without_changing_liveness() {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        async fn status(addr: SocketAddr, path: &str) -> String {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(
                    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response.lines().next().unwrap().to_string()
        }

        timeout(Duration::from_secs(3), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (events_tx, events_rx) = mpsc::channel(1);
            let shutdown = CancellationToken::new();
            let acceptor = start_websocket_acceptor_with_listener(
                listener,
                events_tx,
                shutdown.clone(),
                WebsocketAuthPolicy::default(),
            )
            .unwrap();
            assert_eq!(status(addr, "/readyz").await, "HTTP/1.1 200 OK");
            drop(events_rx);
            assert_eq!(
                status(addr, "/readyz").await,
                "HTTP/1.1 503 Service Unavailable"
            );
            assert_eq!(status(addr, "/healthz").await, "HTTP/1.1 200 OK");
            shutdown.cancel();
            acceptor.await.unwrap();
        })
        .await
        .expect("health routes and shutdown must finish");
    }

    #[tokio::test]
    async fn listener_shutdown_cancels_registration_behind_full_ingress() {
        let writer_dropped = Arc::new(AtomicBool::new(false));
        let reader_dropped = Arc::new(AtomicBool::new(false));
        let (_frame_tx, frames) = mpsc::unbounded_channel();
        let (events_tx, mut events_rx) = mpsc::channel(1);
        events_tx
            .send(TransportEvent::ConnectionClosed {
                connection_id: ConnectionId(123),
            })
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let connection = run_websocket_connection(
            TrackedWriter(Arc::clone(&writer_dropped)),
            TrackedReader {
                frames,
                dropped: Arc::clone(&reader_dropped),
                read_frame: None,
            },
            events_tx,
            shutdown.clone(),
        );
        tokio::pin!(connection);
        assert!(futures::poll!(&mut connection).is_pending());
        shutdown.cancel();
        timeout(Duration::from_secs(1), connection)
            .await
            .expect("registration wait must be cancelable");
        assert!(writer_dropped.load(Ordering::SeqCst));
        assert!(reader_dropped.load(Ordering::SeqCst));
        assert!(matches!(
            events_rx.recv().await,
            Some(TransportEvent::ConnectionClosed {
                connection_id: ConnectionId(123)
            })
        ));
        assert!(matches!(
            events_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    struct TrackedWriter(Arc<AtomicBool>);

    impl Drop for TrackedWriter {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl futures::Sink<TungsteniteWebSocketMessage> for TrackedWriter {
        type Error = std::io::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
            Poll::Ready(Err(std::io::Error::other("peer disconnected")))
        }

        fn start_send(self: Pin<&mut Self>, _: TungsteniteWebSocketMessage) -> IoResult<()> {
            unreachable!("the writer fails before accepting a frame")
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct TrackedReader {
        frames: mpsc::UnboundedReceiver<TungsteniteWebSocketMessage>,
        dropped: Arc<AtomicBool>,
        read_frame: Option<Arc<tokio::sync::Notify>>,
    }

    impl Drop for TrackedReader {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl futures::Stream for TrackedReader {
        type Item = IoResult<TungsteniteWebSocketMessage>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let frame = self.frames.poll_recv(cx);
            if matches!(frame, Poll::Ready(Some(_)))
                && let Some(read_frame) = &self.read_frame
            {
                read_frame.notify_one();
            }
            frame.map(|frame| frame.map(Ok))
        }
    }

    #[tokio::test]
    async fn connection_closed_waits_for_both_websocket_workers() {
        for close_inbound in [true, false] {
            let writer_dropped = Arc::new(AtomicBool::new(false));
            let reader_dropped = Arc::new(AtomicBool::new(false));
            let (frame_tx, frames) = mpsc::unbounded_channel();
            let (transport_event_tx, mut transport_event_rx) = mpsc::channel(8);
            let connection = run_websocket_connection(
                TrackedWriter(Arc::clone(&writer_dropped)),
                TrackedReader {
                    frames,
                    dropped: Arc::clone(&reader_dropped),
                    read_frame: None,
                },
                transport_event_tx,
                CancellationToken::new(),
            );
            let drive_connection = async {
                let (connection_id, writer) = match transport_event_rx.recv().await.unwrap() {
                    TransportEvent::ConnectionOpened {
                        connection_id,
                        writer,
                        ..
                    } => (connection_id, writer),
                    event => panic!("expected connection opened, got {event:?}"),
                };
                if close_inbound {
                    frame_tx
                        .send(TungsteniteWebSocketMessage::Close(None))
                        .unwrap();
                } else {
                    writer
                        .send(QueuedOutgoingMessage::new(OutgoingMessage::Response(
                            OutgoingResponse {
                                id: RequestId::Integer(1),
                                result: serde_json::json!({"ok": true}),
                            },
                        )))
                        .await
                        .unwrap();
                }
                match transport_event_rx.recv().await.unwrap() {
                    TransportEvent::ConnectionClosed {
                        connection_id: closed_id,
                    } => assert_eq!(closed_id, connection_id),
                    event => panic!("expected connection closed, got {event:?}"),
                }
                assert!(writer_dropped.load(Ordering::SeqCst));
                assert!(reader_dropped.load(Ordering::SeqCst));
            };
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                tokio::join!(connection, drive_connection);
            })
            .await
            .expect("connection must close promptly");
        }
    }

    #[tokio::test]
    async fn cancelling_websocket_connection_drops_both_workers() {
        let writer_dropped = Arc::new(AtomicBool::new(false));
        let reader_dropped = Arc::new(AtomicBool::new(false));
        let (frame_tx, frames) = mpsc::unbounded_channel();
        let (transport_event_tx, mut transport_event_rx) = mpsc::channel(8);
        let connection = tokio::spawn(run_websocket_connection(
            TrackedWriter(Arc::clone(&writer_dropped)),
            TrackedReader {
                frames,
                dropped: Arc::clone(&reader_dropped),
                read_frame: None,
            },
            transport_event_tx,
            CancellationToken::new(),
        ));
        let writer = match transport_event_rx.recv().await.unwrap() {
            TransportEvent::ConnectionOpened { writer, .. } => writer,
            event => panic!("expected connection opened, got {event:?}"),
        };
        connection.abort();
        assert!(connection.await.unwrap_err().is_cancelled());
        // Dropping the owner schedules both aborts; channel closure proves the
        // workers actually released their resources before checking effects.
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            writer.closed().await;
            frame_tx.closed().await;
        })
        .await
        .expect("cancelled connection must release both workers");
        assert!(writer_dropped.load(Ordering::SeqCst));
        assert!(reader_dropped.load(Ordering::SeqCst));
        assert!(
            frame_tx
                .send(TungsteniteWebSocketMessage::Text(
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#.into()
                ))
                .is_err()
        );
        assert!(transport_event_rx.recv().await.is_none());
    }

    struct BlockedWriter {
        started: Arc<tokio::sync::Notify>,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for BlockedWriter {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl futures::Sink<TungsteniteWebSocketMessage> for BlockedWriter {
        type Error = std::io::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
            self.started.notify_one();
            Poll::Pending
        }

        fn start_send(self: Pin<&mut Self>, _: TungsteniteWebSocketMessage) -> IoResult<()> {
            panic!("blocked external socket cannot accept a frame")
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
            Poll::Pending
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn peer_close_does_not_wait_indefinitely_for_stalled_flush() {
        timeout(Duration::from_secs(3), async {
            let writer_dropped = Arc::new(AtomicBool::new(false));
            let reader_dropped = Arc::new(AtomicBool::new(false));
            let (frames_tx, frames) = mpsc::unbounded_channel();
            let (events_tx, mut events_rx) = mpsc::channel(8);
            let connection = tokio::spawn(run_websocket_connection(
                BlockedWriter {
                    started: Arc::new(tokio::sync::Notify::new()),
                    dropped: Arc::clone(&writer_dropped),
                },
                TrackedReader {
                    frames,
                    dropped: Arc::clone(&reader_dropped),
                    read_frame: None,
                },
                events_tx,
                CancellationToken::new(),
            ));
            let (connection_id, writer) = match events_rx.recv().await.unwrap() {
                TransportEvent::ConnectionOpened {
                    connection_id,
                    writer,
                    ..
                } => (connection_id, writer),
                event => panic!("expected registered connection: {event:?}"),
            };
            frames_tx
                .send(TungsteniteWebSocketMessage::Close(None))
                .unwrap();
            match events_rx.recv().await.unwrap() {
                TransportEvent::ConnectionClosed { connection_id: id } => {
                    assert_eq!(id, connection_id)
                }
                event => panic!("expected connection closed: {event:?}"),
            }
            connection.await.unwrap();
            assert!(writer.is_closed());
            assert!(writer_dropped.load(Ordering::SeqCst));
            assert!(reader_dropped.load(Ordering::SeqCst));
            assert!(events_rx.recv().await.is_none());
        })
        .await
        .expect("close flush must be bounded without listener cancellation");
    }

    #[tokio::test]
    async fn disconnect_releases_stalled_websocket_io_before_ingress_drains() {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let writer_dropped = Arc::new(AtomicBool::new(false));
            let reader_dropped = Arc::new(AtomicBool::new(false));
            let write_started = Arc::new(tokio::sync::Notify::new());
            let read_frame = Arc::new(tokio::sync::Notify::new());
            let (frame_tx, frames) = mpsc::unbounded_channel();
            let (transport_event_tx, mut transport_event_rx) = mpsc::channel(1);
            let connection = tokio::spawn(run_websocket_connection(
                BlockedWriter {
                    started: Arc::clone(&write_started),
                    dropped: Arc::clone(&writer_dropped),
                },
                TrackedReader {
                    frames,
                    dropped: Arc::clone(&reader_dropped),
                    read_frame: Some(Arc::clone(&read_frame)),
                },
                transport_event_tx.clone(),
                CancellationToken::new(),
            ));
            let (connection_id, writer, disconnect) = match transport_event_rx.recv().await.unwrap()
            {
                TransportEvent::ConnectionOpened {
                    connection_id,
                    writer,
                    disconnect_sender: Some(disconnect),
                    ..
                } => (connection_id, writer, disconnect),
                event => panic!("expected registered connection, got {event:?}"),
            };
            writer
                .send(QueuedOutgoingMessage::new(OutgoingMessage::Response(
                    OutgoingResponse {
                        id: RequestId::Integer(17),
                        result: serde_json::json!("held"),
                    },
                )))
                .await
                .unwrap();
            write_started.notified().await;
            let retained = codex_app_server_protocol::JSONRPCNotification {
                method: "retained".into(),
                params: None,
            };
            transport_event_tx
                .send(TransportEvent::IncomingMessage {
                    connection_id,
                    message: codex_app_server_protocol::JSONRPCMessage::Notification(
                        retained.clone(),
                    ),
                })
                .await
                .unwrap();
            frame_tx
                .send(TungsteniteWebSocketMessage::Text(
                    r#"{"jsonrpc":"2.0","method":"cancelled"}"#.into(),
                ))
                .unwrap();
            read_frame.notified().await;

            // This is the token delivered to the real outbound router at registration.
            // Neither stalled IO operation checks it until its await returns.
            disconnect.cancel();
            writer.closed().await;
            frame_tx.closed().await;
            assert!(writer_dropped.load(Ordering::SeqCst));
            assert!(reader_dropped.load(Ordering::SeqCst));
            match transport_event_rx.recv().await.unwrap() {
                TransportEvent::IncomingMessage { message, .. } => assert_eq!(
                    message,
                    codex_app_server_protocol::JSONRPCMessage::Notification(retained),
                ),
                event => panic!("expected retained message, got {event:?}"),
            }
            match transport_event_rx.recv().await.unwrap() {
                TransportEvent::ConnectionClosed {
                    connection_id: closed_id,
                } => {
                    assert_eq!(closed_id, connection_id);
                }
                event => panic!("cancelled message must not be forwarded: {event:?}"),
            }
            connection.await.unwrap();
            drop(transport_event_tx);
            assert!(transport_event_rx.recv().await.is_none());
        })
        .await
        .expect("disconnect must release both workers while ingress is still full");
    }
}
