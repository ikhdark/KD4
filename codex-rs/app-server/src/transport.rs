use crate::message_processor::ConnectionSessionState;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingWriteReceipt;
use codex_app_server_protocol::ExperimentalApi;
use codex_app_server_protocol::ServerRequest;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub use codex_app_server_transport::AppServerTransport;
pub(crate) use codex_app_server_transport::CHANNEL_CAPACITY;
pub(crate) use codex_app_server_transport::ConnectionId;
pub(crate) use codex_app_server_transport::ConnectionOrigin;
pub(crate) use codex_app_server_transport::OutgoingMessage;
pub(crate) use codex_app_server_transport::QueuedOutgoingMessage;
pub(crate) use codex_app_server_transport::RemoteControlEnableError;
pub(crate) use codex_app_server_transport::RemoteControlHandle;
pub(crate) use codex_app_server_transport::RemoteControlPolicy;
pub(crate) use codex_app_server_transport::RemoteControlStartConfig;
pub use codex_app_server_transport::RemoteControlStartupMode;
pub(crate) use codex_app_server_transport::RemoteControlUnavailable;
pub(crate) use codex_app_server_transport::TransportEvent;
pub(crate) use codex_app_server_transport::acquire_app_server_startup_lock;
pub use codex_app_server_transport::app_server_control_socket_path;
pub(crate) use codex_app_server_transport::app_server_startup_lock_path;
pub use codex_app_server_transport::auth;
pub(crate) use codex_app_server_transport::prepare_control_socket_path;
pub(crate) use codex_app_server_transport::start_control_socket_acceptor;
pub(crate) use codex_app_server_transport::start_remote_control;
pub(crate) use codex_app_server_transport::start_stdio_connection;
pub(crate) use codex_app_server_transport::start_websocket_acceptor;
pub use codex_app_server_transport::take_remote_control_disabled_env;
pub(crate) use codex_app_server_transport::validate_websocket_listener;

pub(crate) struct ConnectionState {
    pub(crate) outbound_initialized: Arc<AtomicBool>,
    pub(crate) outbound_experimental_api_enabled: Arc<AtomicBool>,
    pub(crate) outbound_opted_out_notification_methods: Arc<OutboundNotificationOptOuts>,
    pub(crate) session: Arc<ConnectionSessionState>,
}

impl ConnectionState {
    pub(crate) fn new(
        _origin: ConnectionOrigin,
        outbound_initialized: Arc<AtomicBool>,
        outbound_experimental_api_enabled: Arc<AtomicBool>,
        outbound_opted_out_notification_methods: Arc<OutboundNotificationOptOuts>,
    ) -> Self {
        Self {
            outbound_initialized,
            outbound_experimental_api_enabled,
            outbound_opted_out_notification_methods,
            session: Arc::new(ConnectionSessionState::new()),
        }
    }
}

pub(crate) struct OutboundNotificationOptOuts {
    methods: RwLock<HashSet<String>>,
    has_any: AtomicBool,
}

impl OutboundNotificationOptOuts {
    pub(crate) fn new(methods: HashSet<String>) -> Self {
        let has_any = !methods.is_empty();
        Self {
            methods: RwLock::new(methods),
            has_any: AtomicBool::new(has_any),
        }
    }

    pub(crate) fn replace(&self, methods: HashSet<String>) -> bool {
        let Ok(mut current) = self.methods.write() else {
            return false;
        };
        let has_any = !methods.is_empty();
        *current = methods;
        self.has_any.store(has_any, Ordering::Release);
        true
    }

    fn has_any(&self) -> bool {
        self.has_any.load(Ordering::Acquire)
    }

    fn contains(&self, method: &str) -> Option<bool> {
        self.methods
            .read()
            .ok()
            .map(|methods| methods.contains(method))
    }
}

pub(crate) struct OutboundConnectionState {
    pub(crate) initialized: Arc<AtomicBool>,
    pub(crate) experimental_api_enabled: Arc<AtomicBool>,
    pub(crate) opted_out_notification_methods: Arc<OutboundNotificationOptOuts>,
    pub(crate) writer: mpsc::Sender<QueuedOutgoingMessage>,
    disconnect_sender: Option<CancellationToken>,
    transport_shutdown: CancellationToken,
    stdio_delivery: Option<StdioDelivery>,
}

struct StdioDelivery {
    sender: mpsc::Sender<QueuedOutgoingMessage>,
    bytes: Arc<tokio::sync::Semaphore>,
}

const STDIO_DELIVERY_BYTES: usize = 16 * 1024 * 1024;

impl OutboundConnectionState {
    pub(crate) fn new(
        writer: mpsc::Sender<QueuedOutgoingMessage>,
        initialized: Arc<AtomicBool>,
        experimental_api_enabled: Arc<AtomicBool>,
        opted_out_notification_methods: Arc<OutboundNotificationOptOuts>,
        disconnect_sender: Option<CancellationToken>,
        transport_shutdown: CancellationToken,
    ) -> Self {
        Self {
            initialized,
            experimental_api_enabled,
            opted_out_notification_methods,
            writer,
            disconnect_sender,
            transport_shutdown,
            stdio_delivery: None,
        }
    }

    fn can_disconnect(&self) -> bool {
        self.disconnect_sender.is_some()
    }

    pub(crate) fn request_disconnect(&self) {
        self.disconnect_sender
            .as_ref()
            .unwrap_or(&self.transport_shutdown)
            .cancel();
    }
}

impl Drop for OutboundConnectionState {
    fn drop(&mut self) {
        // The router owns connection retirement even when its control receiver
        // disappears before it can consume an explicit DisconnectAll event.
        self.request_disconnect();
    }
}

fn should_skip_notification_for_connection(
    connection_state: &OutboundConnectionState,
    message: &OutgoingMessage,
) -> bool {
    match message {
        OutgoingMessage::AppServerNotification(notification) => {
            if notification.experimental_reason().is_some()
                && !connection_state
                    .experimental_api_enabled
                    .load(Ordering::Acquire)
            {
                return true;
            }
            // The common case is an empty set. Check its lock-free summary before taking the
            // lock or rendering the method name, which allocates for every stream delta.
            if !connection_state.opted_out_notification_methods.has_any() {
                return false;
            }
            let method = notification.to_string();
            let Some(contains_method) = connection_state
                .opted_out_notification_methods
                .contains(method.as_str())
            else {
                warn!("failed to read outbound opted-out notifications");
                return false;
            };
            contains_method
        }
        _ => false,
    }
}

fn disconnect_connection(
    connections: &mut HashMap<ConnectionId, OutboundConnectionState>,
    connection_id: ConnectionId,
) -> bool {
    if let Some(connection_state) = connections.remove(&connection_id) {
        connection_state.request_disconnect();
        return true;
    }
    false
}

async fn send_message_to_connection(
    connections: &mut HashMap<ConnectionId, OutboundConnectionState>,
    connection_id: ConnectionId,
    message: OutgoingMessage,
    write_complete_tx: Option<OutgoingWriteReceipt>,
    notification_prechecked: bool,
) -> bool {
    let Some(connection_state) = connections.get_mut(&connection_id) else {
        warn!("dropping message for disconnected connection: {connection_id:?}");
        return false;
    };
    let message = filter_outgoing_message_for_connection(connection_state, message);
    if !notification_prechecked && should_skip_notification_for_connection(connection_state, &message) {
        if let Some(receipt) = write_complete_tx
            && let Some(filtered) = receipt.filtered
        {
            let _ = filtered.send(());
        }
        return false;
    }

    let writer = connection_state.writer.clone();
    let mut queued_message = QueuedOutgoingMessage {
        message,
        encoded_json: None,
        write_complete_tx: write_complete_tx.map(|receipt| receipt.writer),
    };
    if connection_state.can_disconnect() {
        match writer.try_send(queued_message) {
            Ok(()) => false,
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(
                    "disconnecting slow connection after outbound queue filled: {connection_id:?}"
                );
                disconnect_connection(connections, connection_id)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                disconnect_connection(connections, connection_id)
            }
        }
    } else {
        // The per-stdio owner absorbs bursts without waiting on the physical
        // writer. When its bounded staging budget fills, backpressure must reach
        // producers instead of cancelling the shared server shutdown token.
        let shutdown = connection_state.transport_shutdown.clone();
        if shutdown.is_cancelled() {
            return disconnect_connection(connections, connection_id);
        }
        let delivery = connection_state.stdio_delivery.get_or_insert_with(|| {
            let (sender, mut receiver) =
                mpsc::channel::<QueuedOutgoingMessage>(128);
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                loop {
                    let next = tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => break,
                        next = receiver.recv() => next,
                    };
                    let Some(message) = next else {
                        break;
                    };
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => break,
                        result = writer.send(message) => {
                            if result.is_err() { shutdown.cancel(); break; }
                        }
                    }
                }
            });
            StdioDelivery {
                sender,
                bytes: Arc::new(tokio::sync::Semaphore::new(STDIO_DELIVERY_BYTES)),
            }
        });
        let bytes = match queued_message.prepare_stdio_json(STDIO_DELIVERY_BYTES) {
            Ok(bytes) => {
                if bytes > STDIO_DELIVERY_BYTES {
                    let (message_kind, request_id) = match &queued_message.message {
                        OutgoingMessage::Response(response) => ("response", Some(&response.id)),
                        OutgoingMessage::Error(error) => ("error", Some(&error.id)),
                        OutgoingMessage::Request(_) => ("request", None),
                        OutgoingMessage::AppServerNotification(_) => ("notification", None),
                    };
                    warn!(
                        ?connection_id,
                        message_kind,
                        ?request_id,
                        encoded_bytes = bytes,
                        "streaming stdio output larger than the staging budget"
                    );
                }
                bytes.clamp(1, STDIO_DELIVERY_BYTES) as u32
            }
            Err(error) => {
                warn!(?connection_id, %error, "failed to encode bounded stdio output");
                return disconnect_connection(connections, connection_id);
            }
        };
        // Both admission waits remain interruptible. Credits stay attached to
        // the frame until its physical write completes, preserving the byte cap.
        // A streamed frame reserves the entire budget so large frames cannot
        // accumulate behind a slow writer or wait for impossible permit counts.
        let credit = tokio::select! {
            biased;
            _ = shutdown.cancelled() => None,
            result = delivery.bytes.clone().acquire_many_owned(bytes) => result.ok(),
        };
        let Some(credit) = credit else {
            return disconnect_connection(connections, connection_id);
        };
        if let Some(encoded) = &mut queued_message.encoded_json {
            encoded.delivery_credit = Some(credit);
        }
        let sent = tokio::select! {
            biased;
            _ = shutdown.cancelled() => false,
            result = delivery.sender.send(queued_message) => result.is_ok(),
        };
        if sent {
            return false;
        }
        // A closed writer or explicit shutdown still retires the connection;
        // temporary count/byte saturation does not.
        disconnect_connection(connections, connection_id)
    }
}

fn filter_outgoing_message_for_connection(
    connection_state: &OutboundConnectionState,
    message: OutgoingMessage,
) -> OutgoingMessage {
    let experimental_api_enabled = connection_state
        .experimental_api_enabled
        .load(Ordering::Acquire);
    match message {
        OutgoingMessage::Request(ServerRequest::CommandExecutionRequestApproval {
            request_id,
            mut params,
        }) => {
            if !experimental_api_enabled {
                params.strip_experimental_fields();
            }
            OutgoingMessage::Request(ServerRequest::CommandExecutionRequestApproval {
                request_id,
                params,
            })
        }
        _ => message,
    }
}

pub(crate) async fn route_outgoing_envelope(
    connections: &mut HashMap<ConnectionId, OutboundConnectionState>,
    envelope: OutgoingEnvelope,
) {
    let notification_prechecked = matches!(&envelope, OutgoingEnvelope::Broadcast { .. });
    let target_connection_ids = target_connection_ids(connections, &envelope);
    let (message, mut write_complete_tx) = match envelope {
        OutgoingEnvelope::ToConnection {
            message,
            write_complete_tx,
            ..
        } => (message, write_complete_tx),
        OutgoingEnvelope::Broadcast { message } => (message, None),
    };

    // Hand the value to the final recipient instead of cloning for it. Single-connection
    // delivery, which is the common case, then performs no deep clone at all.
    let mut message = Some(message);
    let target_count = target_connection_ids.len();
    for (index, connection_id) in target_connection_ids.into_iter().enumerate() {
        let is_last = index + 1 == target_count;
        let outgoing = if is_last {
            match message.take() {
                Some(outgoing) => outgoing,
                None => break,
            }
        } else {
            match message.as_ref() {
                Some(outgoing) => outgoing.clone(),
                None => break,
            }
        };
        let _ = send_message_to_connection(
            connections,
            connection_id,
            outgoing,
            write_complete_tx.take(),
            notification_prechecked,
        )
        .await;
    }
}

fn target_connection_ids(
    connections: &HashMap<ConnectionId, OutboundConnectionState>,
    envelope: &OutgoingEnvelope,
) -> Vec<ConnectionId> {
    match envelope {
        OutgoingEnvelope::ToConnection { connection_id, .. } => vec![*connection_id],
        OutgoingEnvelope::Broadcast { message } => connections
            .iter()
            .filter_map(|(connection_id, connection_state)| {
                // Snapshot notification eligibility once, before cloning the
                // payload. Later mutable opt-out/capability updates apply to
                // subsequent envelopes, not a second check of this one.
                (connection_state.initialized.load(Ordering::Acquire)
                    && !should_skip_notification_for_connection(connection_state, message))
                    .then_some(*connection_id)
            })
            .collect(),
    }
}

