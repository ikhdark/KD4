pub mod auth;

use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::QueuedOutgoingMessage;
use codex_app_server_protocol_types::JSONRPCMessage;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_home_dir::find_codex_home;
use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
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
const REJECTION_INGRESS_RESERVE: usize = 64;
pub const INGRESS_CAPACITY: usize =
    CHANNEL_CAPACITY + REJECTION_INGRESS_RESERVE + CONTROL_INGRESS_RESERVE;

/// Sender for a bounded channel with capacity reserved for control traffic.
pub struct ControlReserveSender<T> {
    sender: mpsc::Sender<T>,
    admission: Arc<Mutex<()>>,
}

impl<T> Clone for ControlReserveSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            admission: Arc::clone(&self.admission),
        }
    }
}

impl<T> ControlReserveSender<T> {
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<T>) {
        let (sender, receiver) = mpsc::channel(capacity);
        let sender = Self {
            sender,
            admission: Arc::new(Mutex::new(())),
        };
        (sender, receiver)
    }

    /// Serialize the capacity check and reservation across this channel's
    /// cooperating producers. Ordinary sends never borrow a control cell.
    pub fn try_send_with_control_reserve(
        &self,
        message: T,
        control: bool,
        reserve: usize,
    ) -> Result<(), mpsc::error::TrySendError<T>> {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reserve = reserve.min(self.max_capacity().saturating_sub(1));
        if self.is_closed() {
            return Err(mpsc::error::TrySendError::Closed(message));
        }
        if !control && self.sender.capacity() <= reserve {
            return Err(mpsc::error::TrySendError::Full(message));
        }
        let permit = match self.sender.try_reserve() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(())) => {
                return Err(mpsc::error::TrySendError::Full(message));
            }
            Err(mpsc::error::TrySendError::Closed(())) => {
                return Err(mpsc::error::TrySendError::Closed(message));
            }
        };
        permit.send(message);
        Ok(())
    }

    /// Lifecycle send that may use any available cell without admission locking.
    pub async fn send_unreserved(&self, message: T) -> Result<(), mpsc::error::SendError<T>> {
        self.sender.send(message).await
    }

    fn try_send_unreserved(&self, message: T) -> Result<(), mpsc::error::TrySendError<T>> {
        self.sender.try_send(message)
    }

    fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    fn max_capacity(&self) -> usize {
        self.sender.max_capacity()
    }
}

mod remote_control;
mod stdio;
mod unix_socket;
mod websocket;

pub use remote_control::REMOTE_CONTROL_DISABLED_ENV_VAR;
pub use remote_control::REMOTE_PREVIEW_FIELD_BYTES;
pub use remote_control::RemoteControlDisabledByRequirements;
pub use remote_control::RemoteControlEnableError;
pub use remote_control::RemoteControlHandle;
pub use remote_control::RemoteControlPolicy;
pub use remote_control::RemoteControlStartConfig;
pub use remote_control::RemoteControlStartupMode;
pub use remote_control::RemoteControlUnavailable;
pub use remote_control::observe_remote_response;
pub use remote_control::preview_remote_notification;
pub use remote_control::remote_control_response_size_error;
pub use remote_control::remote_control_retry_details;
pub use remote_control::remote_control_user_agent;
pub use remote_control::rpc_request_key_v1;
pub use remote_control::start_remote_control;
pub use remote_control::take_remote_control_disabled_env;
pub use stdio::note_stdio_output_retirement;
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
    RequestRejected {
        connection_id: ConnectionId,
        request_id: codex_app_server_protocol_types::RequestId,
    },
    IncomingMessage {
        connection_id: ConnectionId,
        message: JSONRPCMessage,
        // Released after the shared dispatcher transfers or consumes this message.
        _ingress_permit: Option<tokio::sync::OwnedSemaphorePermit>,
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
    transport_event_tx: &ControlReserveSender<TransportEvent>,
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
    transport_event_tx: &ControlReserveSender<TransportEvent>,
    writer: &mpsc::Sender<QueuedOutgoingMessage>,
    connection_id: ConnectionId,
    message: JSONRPCMessage,
) -> bool {
    enqueue_incoming_message_with_admission(transport_event_tx, writer, connection_id, message).await.0
}

async fn enqueue_incoming_message_with_admission(
    transport_event_tx: &ControlReserveSender<TransportEvent>,
    _writer: &mpsc::Sender<QueuedOutgoingMessage>,
    connection_id: ConnectionId,
    message: JSONRPCMessage,
) -> (bool, bool) {
    use std::sync::LazyLock;
    use tokio::sync::Semaphore;
    const BYTE_BUDGET: usize = 128 * 1024 * 1024;
    // Shared by every external connection; ordinary traffic cannot borrow the
    // control byte budget any more than it can borrow reserved channel cells.
    static ORDINARY_BYTES: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(BYTE_BUDGET)));
    static CONTROL_BYTES: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(BYTE_BUDGET)));
    let control = ingress_message_is_control(&message);
    let bytes = serialized_request_bytes_up_to(&message, BYTE_BUDGET);
    let budget = if control { &CONTROL_BYTES } else { &ORDINARY_BYTES };
    let permit = u32::try_from(bytes.max(1)).ok()
        .filter(|_| bytes <= BYTE_BUDGET)
        .and_then(|bytes| Arc::clone(budget).try_acquire_many_owned(bytes).ok());
    let admitted_bytes = permit.is_some();
    let event = TransportEvent::IncomingMessage {
        connection_id,
        message,
        _ingress_permit: permit,
    };
    let result = if admitted_bytes {
        transport_event_tx.try_send_with_control_reserve(
            event,
            control,
            ingress_reserve(transport_event_tx),
        )
    } else {
        Err(mpsc::error::TrySendError::Full(event))
    };
    match result {
        Ok(()) => (true, true),
        Err(mpsc::error::TrySendError::Closed(_)) => (false, false),
        Err(mpsc::error::TrySendError::Full(TransportEvent::IncomingMessage {
            connection_id,
            message: JSONRPCMessage::Request(request),
            ..
        })) => {
            // The RPC owner must claim the ID, including collisions with live
            // requests. Rejection bookkeeping can use its own reserve but
            // must not consume cells reserved for actual control messages.
            if serialized_request_bytes_up_to(&request.id, 64 * 1024) > 64 * 1024 {
                warn!(?connection_id, "closing connection: overload response ID exceeds the ingress rejection bound");
                return (false, false);
            }
            let reserve = if transport_event_tx.max_capacity() >= INGRESS_CAPACITY {
                CONTROL_INGRESS_RESERVE
            } else {
                0
            };
            match transport_event_tx.try_send_with_control_reserve(
                TransportEvent::RequestRejected { connection_id, request_id: request.id },
                false,
                reserve,
            ) {
                Ok(()) => (true, false),
                Err(mpsc::error::TrySendError::Closed(_)) => (false, false),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        "closing connection {:?}: outbound queue cannot accept overload response",
                        connection_id
                    );
                    (false, false)
                }
            }
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            // Notifications cannot be rejected with an RPC error, and replies
            // must never be silently discarded. Explicitly retire this overloaded
            // connection rather than borrowing the interrupt reserve or blocking
            // the reader behind an unreserved send.
            warn!("closing connection {connection_id:?}: ingress cannot accept required message");
            (false, false)
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
            codex_app_server_protocol_types::client_request_is_control(&request.method)
                && serialized_request_bytes_up_to(request, 64 * 1024) <= 64 * 1024
        }
    }
}

fn ingress_reserve(sender: &ControlReserveSender<TransportEvent>) -> usize {
    if sender.max_capacity() >= INGRESS_CAPACITY {
        REJECTION_INGRESS_RESERVE + CONTROL_INGRESS_RESERVE
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

