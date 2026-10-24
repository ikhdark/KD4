pub mod auth;

use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingError;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::QueuedOutgoingMessage;
use codex_app_server_protocol::JSONRPCMessage;
#[cfg(test)]
use codex_app_server_protocol::OVERLOADED_ERROR_CODE;
use codex_app_server_protocol::OverloadReason;
use codex_app_server_protocol::overloaded_error;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_home_dir::find_codex_home;
use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::warn;

/// Size of the bounded channels used to communicate between tasks. The value
/// is a balance between throughput and memory usage - 128 messages should be
/// plenty for an interactive CLI.
pub const CHANNEL_CAPACITY: usize = 128;
pub const CONTROL_INGRESS_RESERVE: usize = 64;
pub const INGRESS_CAPACITY: usize = CHANNEL_CAPACITY + CONTROL_INGRESS_RESERVE;

/// Reserve atomically before checking the remaining ordinary allowance. A
/// concurrent sender cannot consume the last control cell between check/send.
pub fn try_send_with_control_reserve<T>(
    sender: &mpsc::Sender<T>,
    message: T,
    control: bool,
    reserve: usize,
) -> Result<(), mpsc::error::TrySendError<T>> {
    let permit = match sender.try_reserve() {
        Ok(permit) => permit,
        Err(mpsc::error::TrySendError::Full(())) => {
            return Err(mpsc::error::TrySendError::Full(message));
        }
        Err(mpsc::error::TrySendError::Closed(())) => {
            return Err(mpsc::error::TrySendError::Closed(message));
        }
    };
    // Legacy callers/tests with a single cell still have ordinary admission.
    let reserve = reserve.min(sender.max_capacity().saturating_sub(1));
    if !control && sender.capacity() < reserve {
        return Err(mpsc::error::TrySendError::Full(message));
    }
    permit.send(message);
    Ok(())
}

mod remote_control;
mod stdio;
mod unix_socket;
#[cfg(test)]
mod unix_socket_tests;
mod websocket;

pub use remote_control::REMOTE_CONTROL_DISABLED_ENV_VAR;
pub use remote_control::RemoteControlDisabledByRequirements;
pub use remote_control::RemoteControlEnableError;
pub use remote_control::RemoteControlHandle;
pub use remote_control::RemoteControlPolicy;
pub use remote_control::RemoteControlStartConfig;
pub use remote_control::RemoteControlStartupMode;
pub use remote_control::RemoteControlUnavailable;
pub use remote_control::start_remote_control;
pub use remote_control::take_remote_control_disabled_env;
pub use stdio::start_stdio_connection;
pub use unix_socket::AppServerStartupLock;
pub use unix_socket::acquire_app_server_startup_lock;
pub use unix_socket::prepare_control_socket_path;
pub use unix_socket::start_control_socket_acceptor;
pub use websocket::start_websocket_acceptor;
pub use websocket::validate_websocket_listener;

const APP_SERVER_CONTROL_SOCKET_DIR_NAME: &str = "app-server-control";
const APP_SERVER_CONTROL_SOCKET_FILE_NAME: &str = "app-server-control.sock";
const APP_SERVER_STARTUP_LOCK_FILE_NAME: &str = "app-server-startup.lock";

pub fn app_server_control_socket_path(codex_home: &Path) -> std::io::Result<AbsolutePathBuf> {
    AbsolutePathBuf::from_absolute_path(
        codex_home
            .join(APP_SERVER_CONTROL_SOCKET_DIR_NAME)
            .join(APP_SERVER_CONTROL_SOCKET_FILE_NAME),
    )
}

pub fn app_server_startup_lock_path(codex_home: &Path) -> std::io::Result<AbsolutePathBuf> {
    AbsolutePathBuf::from_absolute_path(
        codex_home
            .join(APP_SERVER_CONTROL_SOCKET_DIR_NAME)
            .join(APP_SERVER_STARTUP_LOCK_FILE_NAME),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppServerTransport {
    Stdio,
    UnixSocket { socket_path: AbsolutePathBuf },
    WebSocket { bind_address: SocketAddr },
    Off,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum AppServerTransportParseError {
    UnsupportedListenUrl(String),
    InvalidUnixSocketPath { listen_url: String, message: String },
    InvalidWebSocketListenUrl(String),
}

impl std::fmt::Display for AppServerTransportParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppServerTransportParseError::UnsupportedListenUrl(listen_url) => write!(
                f,
                "unsupported --listen URL `{listen_url}`; expected `stdio://`, `unix://`, `unix://PATH`, `ws://IP:PORT`, or `off`"
            ),
            AppServerTransportParseError::InvalidUnixSocketPath {
                listen_url,
                message,
            } => write!(
                f,
                "invalid unix socket --listen URL `{listen_url}`; failed to resolve socket path: {message}"
            ),
            AppServerTransportParseError::InvalidWebSocketListenUrl(listen_url) => write!(
                f,
                "invalid websocket --listen URL `{listen_url}`; expected `ws://IP:PORT`"
            ),
        }
    }
}

impl std::error::Error for AppServerTransportParseError {}

impl AppServerTransport {
    pub const DEFAULT_LISTEN_URL: &'static str = "stdio://";

    pub fn from_listen_url(listen_url: &str) -> Result<Self, AppServerTransportParseError> {
        if listen_url == Self::DEFAULT_LISTEN_URL {
            return Ok(Self::Stdio);
        }

        if let Some(raw_socket_path) = listen_url.strip_prefix("unix://") {
            let socket_path = if raw_socket_path.is_empty() {
                let codex_home = find_codex_home().map_err(|err| {
                    AppServerTransportParseError::InvalidUnixSocketPath {
                        listen_url: listen_url.to_string(),
                        message: format!("failed to resolve CODEX_HOME: {err}"),
                    }
                })?;
                app_server_control_socket_path(&codex_home).map_err(|err| {
                    AppServerTransportParseError::InvalidUnixSocketPath {
                        listen_url: listen_url.to_string(),
                        message: err.to_string(),
                    }
                })?
            } else {
                AbsolutePathBuf::relative_to_current_dir(raw_socket_path).map_err(|err| {
                    AppServerTransportParseError::InvalidUnixSocketPath {
                        listen_url: listen_url.to_string(),
                        message: err.to_string(),
                    }
                })?
            };
            return Ok(Self::UnixSocket { socket_path });
        }

        if listen_url == "off" {
            return Ok(Self::Off);
        }

        if let Some(socket_addr) = listen_url.strip_prefix("ws://") {
            let bind_address = socket_addr.parse::<SocketAddr>().map_err(|_| {
                AppServerTransportParseError::InvalidWebSocketListenUrl(listen_url.to_string())
            })?;
            return Ok(Self::WebSocket { bind_address });
        }

        Err(AppServerTransportParseError::UnsupportedListenUrl(
            listen_url.to_string(),
        ))
    }
}

impl FromStr for AppServerTransport {
    type Err = AppServerTransportParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_listen_url(s)
    }
}

#[derive(Debug)]
pub enum TransportEvent {
    ConnectionOpened {
        connection_id: ConnectionId,
        origin: ConnectionOrigin,
        writer: mpsc::Sender<QueuedOutgoingMessage>,
        disconnect_sender: Option<CancellationToken>,
    },
    ConnectionClosed {
        connection_id: ConnectionId,
    },
    IncomingMessage {
        connection_id: ConnectionId,
        message: JSONRPCMessage,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionOrigin {
    Stdio,
    InProcess,
    WebSocket,
    RemoteControl,
}

static CONNECTION_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_connection_id() -> ConnectionId {
    ConnectionId(CONNECTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
}

async fn forward_incoming_message(
    transport_event_tx: &mpsc::Sender<TransportEvent>,
    writer: &mpsc::Sender<QueuedOutgoingMessage>,
    connection_id: ConnectionId,
    payload: &str,
) -> bool {
    match serde_json::from_str::<JSONRPCMessage>(payload) {
        Ok(message) => {
            enqueue_incoming_message(transport_event_tx, writer, connection_id, message).await
        }
        Err(err) => {
            error!("Failed to deserialize JSONRPCMessage: {err}");
            true
        }
    }
}

async fn enqueue_incoming_message(
    transport_event_tx: &mpsc::Sender<TransportEvent>,
    writer: &mpsc::Sender<QueuedOutgoingMessage>,
    connection_id: ConnectionId,
    message: JSONRPCMessage,
) -> bool {
    let control = ingress_message_is_control(&message);
    let event = TransportEvent::IncomingMessage {
        connection_id,
        message,
    };
    match try_send_with_control_reserve(
        transport_event_tx,
        event,
        control,
        ingress_reserve(transport_event_tx),
    ) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Closed(_)) => false,
        Err(mpsc::error::TrySendError::Full(TransportEvent::IncomingMessage {
            connection_id,
            message: JSONRPCMessage::Request(request),
        })) => {
            let overload_error = OutgoingMessage::Error(OutgoingError {
                id: request.id,
                error: overloaded_error(
                    OverloadReason::TransportIngress,
                    "Server overloaded; retry later.",
                ),
            });
            match writer.try_send(QueuedOutgoingMessage::new(overload_error)) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Closed(_)) => false,
                Err(mpsc::error::TrySendError::Full(_overload_error)) => {
                    warn!(
                        "closing connection {:?}: outbound queue cannot accept overload response",
                        connection_id
                    );
                    false
                }
            }
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            // Notifications cannot be rejected with an RPC error, and replies
            // must never be silently discarded. Explicitly retire this overloaded
            // connection rather than borrowing the interrupt reserve or blocking
            // the reader behind an unreserved send.
            warn!("closing connection {connection_id:?}: ingress cannot accept required message");
            false
        }
    }
}

/// Count a JSON envelope without allocating its wire representation. Stop as
/// soon as it exceeds the budget; serialization failures never gain admission.
pub fn serialized_request_bytes_up_to<T: serde::Serialize + ?Sized>(request: &T, limit: usize) -> usize {
    struct CountingWriter {
        bytes: usize,
        limit: usize,
    }

    impl std::io::Write for CountingWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.bytes = self.bytes.saturating_add(buffer.len());
            if self.bytes > self.limit {
                return Err(std::io::Error::other("request byte budget exceeded"));
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut writer = CountingWriter { bytes: 0, limit };
    serde_json::to_writer(&mut writer, request)
        .map(|()| writer.bytes)
        .unwrap_or(usize::MAX)
}

fn ingress_message_is_control(message: &JSONRPCMessage) -> bool {
    match message {
        JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_) => true,
        JSONRPCMessage::Notification(notification) => notification.method == "initialized",
        JSONRPCMessage::Request(request) => {
            codex_app_server_protocol::client_request_is_control(&request.method)
                && serialized_request_bytes_up_to(request, 64 * 1024) <= 64 * 1024
        }
    }
}

fn ingress_reserve(sender: &mpsc::Sender<TransportEvent>) -> usize {
    if sender.max_capacity() >= INGRESS_CAPACITY {
        CONTROL_INGRESS_RESERVE
    } else {
        0
    }
}

fn serialize_outgoing_message(outgoing_message: OutgoingMessage) -> Option<String> {
    match serde_json::to_string(&outgoing_message) {
        Ok(json) => Some(json),
        Err(err) => {
            error!("Failed to serialize OutgoingMessage: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outgoing_message::OutgoingResponse;
    use codex_app_server_protocol::ConfigWarningNotification;
    use codex_app_server_protocol::JSONRPCNotification;
    use codex_app_server_protocol::JSONRPCRequest;
    use codex_app_server_protocol::JSONRPCResponse;
    use codex_app_server_protocol::RequestId;
    use codex_app_server_protocol::ServerNotification;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn ordinary_ingress_cannot_consume_interrupt_reserve() {
        let (events, mut received) = mpsc::channel(INGRESS_CAPACITY);
        let (writer, mut replies) = mpsc::channel(2);
        let ordinary = |id| {
            JSONRPCMessage::Request(JSONRPCRequest {
                id: RequestId::Integer(id),
                method: "config/read".into(),
                params: Some(json!({"includeLayers": false})),
                trace: None,
            })
        };
        for id in 0..CHANNEL_CAPACITY as i64 {
            assert!(
                enqueue_incoming_message(&events, &writer, ConnectionId(1), ordinary(id)).await
            );
        }
        assert!(enqueue_incoming_message(&events, &writer, ConnectionId(1), ordinary(999)).await);
        assert!(
            replies.try_recv().is_ok(),
            "ordinary overflow stays explicit"
        );
        for id in 0..CONTROL_INGRESS_RESERVE as i64 {
            assert!(
                enqueue_incoming_message(
                    &events,
                    &writer,
                    ConnectionId(2),
                    JSONRPCMessage::Request(JSONRPCRequest {
                        id: RequestId::Integer(1000 + id),
                        method: "turn/interrupt".into(),
                        params: Some(json!({"threadId": "t", "turnId": "turn"})),
                        trace: None,
                    })
                )
                .await
            );
        }
        assert_eq!(received.len(), INGRESS_CAPACITY);
        for _ in 0..CHANNEL_CAPACITY {
            received.recv().await.unwrap();
        }
        assert!(matches!(
            received.recv().await,
            Some(TransportEvent::IncomingMessage {
                connection_id: ConnectionId(2),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn malformed_rpc_envelopes_do_not_reach_dispatch() {
        let (events, mut received) = mpsc::channel(4);
        let (writer, mut outgoing) = mpsc::channel(4);
        let connection_id = ConnectionId(42);
        for payload in [
            r#"{"id":{},"method":"initialized"}"#,
            r#"{"id":1,"result":null,"error":{"code":-1,"message":"failure"}}"#,
        ] {
            assert!(forward_incoming_message(&events, &writer, connection_id, payload).await);
            assert!(matches!(
                received.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            assert!(matches!(
                outgoing.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
        assert!(
            forward_incoming_message(
                &events,
                &writer,
                connection_id,
                r#"{"method":"initialized"}"#
            )
            .await
        );
        assert!(
            matches!(received.try_recv(), Ok(TransportEvent::IncomingMessage {
            message: JSONRPCMessage::Notification(JSONRPCNotification { method, .. }), ..
        }) if method == "initialized")
        );
    }

    #[test]
    fn listen_off_parses_as_off_transport() {
        assert_eq!(
            AppServerTransport::from_listen_url("off"),
            Ok(AppServerTransport::Off)
        );
    }

    #[test]
    fn serialize_outgoing_message_preserves_response_wire_format() {
        let message = OutgoingMessage::Response(OutgoingResponse {
            id: RequestId::Integer(7),
            result: json!({ "ok": true }),
        });

        assert_eq!(
            serialize_outgoing_message(message).as_deref(),
            Some(r#"{"id":7,"result":{"ok":true}}"#)
        );
    }

    #[tokio::test]
    async fn enqueue_incoming_request_returns_overload_error_when_queue_is_full() {
        let connection_id = ConnectionId(42);
        let (transport_event_tx, mut transport_event_rx) = mpsc::channel(1);
        let (writer_tx, mut writer_rx) = mpsc::channel(1);

        let first_message = JSONRPCMessage::Notification(JSONRPCNotification {
            method: "initialized".to_string(),
            params: None,
        });
        transport_event_tx
            .send(TransportEvent::IncomingMessage {
                connection_id,
                message: first_message.clone(),
            })
            .await
            .expect("queue should accept first message");

        let request = JSONRPCMessage::Request(JSONRPCRequest {
            id: RequestId::Integer(7),
            method: "config/read".to_string(),
            params: Some(json!({ "includeLayers": false })),
            trace: None,
        });
        assert!(
            enqueue_incoming_message(&transport_event_tx, &writer_tx, connection_id, request).await
        );

        let queued_event = transport_event_rx
            .recv()
            .await
            .expect("first event should stay queued");
        match queued_event {
            TransportEvent::IncomingMessage {
                connection_id: queued_connection_id,
                message,
            } => {
                assert_eq!(queued_connection_id, connection_id);
                assert_eq!(message, first_message);
            }
            _ => panic!("expected queued incoming message"),
        }

        let overload = writer_rx
            .recv()
            .await
            .expect("request should receive overload error");
        let overload_json =
            serde_json::to_value(overload.message).expect("serialize overload error");
        assert_eq!(
            overload_json,
            json!({
                "id": 7,
                "error": {
                    "code": OVERLOADED_ERROR_CODE,
                    "message": "Server overloaded; retry later.",
                    "data": {
                        "reason": "transportIngress",
                        "retryable": true
                    }
                }
            })
        );
    }

    #[tokio::test]
    async fn required_response_exhaustion_explicitly_closes_instead_of_blocking() {
        let connection_id = ConnectionId(42);
        let (transport_event_tx, mut transport_event_rx) = mpsc::channel(1);
        let (writer_tx, _writer_rx) = mpsc::channel(1);

        let first_message = JSONRPCMessage::Notification(JSONRPCNotification {
            method: "initialized".to_string(),
            params: None,
        });
        transport_event_tx
            .send(TransportEvent::IncomingMessage {
                connection_id,
                message: first_message.clone(),
            })
            .await
            .expect("queue should accept first message");

        let response = JSONRPCMessage::Response(JSONRPCResponse {
            id: RequestId::Integer(7),
            result: json!({"ok": true}),
        });
        assert!(
            !enqueue_incoming_message(&transport_event_tx, &writer_tx, connection_id, response)
                .await
        );

        let queued_event = transport_event_rx
            .recv()
            .await
            .expect("first event should be dequeued");
        match queued_event {
            TransportEvent::IncomingMessage {
                connection_id: queued_connection_id,
                message,
            } => {
                assert_eq!(queued_connection_id, connection_id);
                assert_eq!(message, first_message);
            }
            _ => panic!("expected queued incoming message"),
        }

        assert!(transport_event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn ordinary_notifications_preserve_control_reply_and_lifecycle_reserve() {
        let (events, mut received) = mpsc::channel(INGRESS_CAPACITY);
        let (writer, _replies) = mpsc::channel(1);
        let ordinary = || {
            JSONRPCMessage::Notification(JSONRPCNotification {
                method: "ordinary".into(),
                params: None,
            })
        };
        for _ in 0..CHANNEL_CAPACITY {
            assert!(enqueue_incoming_message(&events, &writer, ConnectionId(1), ordinary()).await);
        }
        for _ in 0..CONTROL_INGRESS_RESERVE {
            assert!(!enqueue_incoming_message(&events, &writer, ConnectionId(2), ordinary()).await);
        }
        for payload in [
            r#"{"id":1,"result":{"ok":true}}"#,
            r#"{"id":2,"error":{"code":-1,"message":"declined"}}"#,
            r#"{"method":"initialized"}"#,
            r#"{"id":3,"method":"turn/interrupt","params":{"threadId":"t","turnId":"turn"}}"#,
        ] {
            assert!(forward_incoming_message(&events, &writer, ConnectionId(3), payload).await);
        }
        assert_eq!(received.len(), CHANNEL_CAPACITY + 4);
        for _ in 0..CHANNEL_CAPACITY {
            received.recv().await.unwrap();
        }
        for _ in 0..4 {
            assert!(matches!(
                received.recv().await,
                Some(TransportEvent::IncomingMessage {
                    connection_id: ConnectionId(3),
                    ..
                })
            ));
        }
    }

    #[tokio::test]
    async fn enqueue_incoming_request_does_not_block_when_writer_queue_is_full() {
        let connection_id = ConnectionId(42);
        let (transport_event_tx, _transport_event_rx) = mpsc::channel(1);
        let (writer_tx, mut writer_rx) = mpsc::channel(1);

        transport_event_tx
            .send(TransportEvent::IncomingMessage {
                connection_id,
                message: JSONRPCMessage::Notification(JSONRPCNotification {
                    method: "initialized".to_string(),
                    params: None,
                }),
            })
            .await
            .expect("transport queue should accept first message");

        writer_tx
            .send(QueuedOutgoingMessage::new(
                OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
                    ConfigWarningNotification {
                        summary: "queued".to_string(),
                        details: None,
                        path: None,
                        range: None,
                    },
                )),
            ))
            .await
            .expect("writer queue should accept first message");

        let request = JSONRPCMessage::Request(JSONRPCRequest {
            id: RequestId::Integer(7),
            method: "config/read".to_string(),
            params: Some(json!({ "includeLayers": false })),
            trace: None,
        });

        let enqueue_result = timeout(
            Duration::from_millis(100),
            enqueue_incoming_message(&transport_event_tx, &writer_tx, connection_id, request),
        )
        .await
        .expect("enqueue should not block while writer queue is full");
        assert!(
            !enqueue_result,
            "connection must close when rejection cannot be delivered"
        );

        let queued_outgoing = writer_rx
            .recv()
            .await
            .expect("writer queue should still contain original message");
        let queued_json =
            serde_json::to_value(queued_outgoing.message).expect("serialize queued message");
        assert_eq!(
            queued_json,
            json!({
                "method": "configWarning",
                "params": {
                    "summary": "queued",
                    "details": null,
                },
            })
        );
    }
}
