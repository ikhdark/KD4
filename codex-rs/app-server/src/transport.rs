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
    #[cfg(test)]
    lock_reads: std::sync::atomic::AtomicUsize,
}

impl OutboundNotificationOptOuts {
    pub(crate) fn new(methods: HashSet<String>) -> Self {
        let has_any = !methods.is_empty();
        Self {
            methods: RwLock::new(methods),
            has_any: AtomicBool::new(has_any),
            #[cfg(test)]
            lock_reads: std::sync::atomic::AtomicUsize::new(0),
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
        #[cfg(test)]
        self.lock_reads.fetch_add(1, Ordering::Relaxed);
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
    sender: mpsc::Sender<(QueuedOutgoingMessage, tokio::sync::OwnedSemaphorePermit)>,
    bytes: Arc<tokio::sync::Semaphore>,
}

const STDIO_DELIVERY_BYTES: usize = 16 * 1024 * 1024;

fn stdio_message_bytes(message: &OutgoingMessage) -> Option<u32> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > STDIO_DELIVERY_BYTES {
                return Err(std::io::Error::other("stdio delivery byte budget exceeded"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, message).ok()?;
    Some(counter.0.max(1) as u32)
}

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
) -> bool {
    let Some(connection_state) = connections.get_mut(&connection_id) else {
        warn!("dropping message for disconnected connection: {connection_id:?}");
        return false;
    };
    let message = filter_outgoing_message_for_connection(connection_state, message);
    if should_skip_notification_for_connection(connection_state, &message) {
        if let Some(receipt) = write_complete_tx
            && let Some(filtered) = receipt.filtered
        {
            let _ = filtered.send(());
        }
        return false;
    }

    let writer = connection_state.writer.clone();
    let queued_message = QueuedOutgoingMessage {
        message,
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
        // The per-stdio owner may wait on its physical writer; the global
        // router never does. Count and bytes both bound this additional stage.
        if connection_state.transport_shutdown.is_cancelled() {
            return disconnect_connection(connections, connection_id);
        }
        let delivery = connection_state.stdio_delivery.get_or_insert_with(|| {
            let (sender, mut receiver) =
                mpsc::channel::<(QueuedOutgoingMessage, tokio::sync::OwnedSemaphorePermit)>(128);
            let shutdown = connection_state.transport_shutdown.clone();
            tokio::spawn(async move {
                loop {
                    let next = tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => break,
                        next = receiver.recv() => next,
                    };
                    let Some((message, credit)) = next else {
                        break;
                    };
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => break,
                        result = writer.send(message) => {
                            if result.is_err() { shutdown.cancel(); break; }
                        }
                    }
                    drop(credit);
                }
            });
            StdioDelivery {
                sender,
                bytes: Arc::new(tokio::sync::Semaphore::new(STDIO_DELIVERY_BYTES)),
            }
        });
        let credit = stdio_message_bytes(&queued_message.message)
            .and_then(|bytes| delivery.bytes.clone().try_acquire_many_owned(bytes).ok());
        if let Some(credit) = credit
            && delivery.sender.try_send((queued_message, credit)).is_ok()
        {
            return false;
        }
        // Required output cannot be silently dropped. Stdio retains its
        // existing process-lifetime failure policy when its owned budget ends.
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
        OutgoingEnvelope::Broadcast { .. } => connections
            .iter()
            .filter_map(|(connection_id, connection_state)| {
                connection_state
                    .initialized
                    .load(Ordering::Acquire)
                    .then_some(*connection_id)
            })
            .collect(),
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
