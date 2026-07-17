use crate::message_processor::ConnectionSessionState;
use crate::outgoing_message::OutgoingEnvelope;
use codex_app_server_protocol::ExperimentalApi;
use codex_app_server_protocol::ServerNotificationDeliveryClass;
use codex_app_server_protocol::ServerRequest;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::sync::Notify;
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

pub(crate) struct ConnectionState {
    pub(crate) outbound_initialized: Arc<AtomicBool>,
    pub(crate) outbound_experimental_api_enabled: Arc<AtomicBool>,
    pub(crate) outbound_opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
    pub(crate) session: Arc<ConnectionSessionState>,
}

impl ConnectionState {
    pub(crate) fn new(
        _origin: ConnectionOrigin,
        outbound_initialized: Arc<AtomicBool>,
        outbound_experimental_api_enabled: Arc<AtomicBool>,
        outbound_opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
    ) -> Self {
        Self {
            outbound_initialized,
            outbound_experimental_api_enabled,
            outbound_opted_out_notification_methods,
            session: Arc::new(ConnectionSessionState::new()),
        }
    }
}

pub(crate) struct OutboundConnectionState {
    pub(crate) initialized: Arc<AtomicBool>,
    pub(crate) experimental_api_enabled: Arc<AtomicBool>,
    pub(crate) opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
    mailbox: OutboundMailbox,
    disconnect_sender: Option<CancellationToken>,
}

#[derive(Clone)]
struct OutboundMailbox {
    writer: mpsc::Sender<QueuedOutgoingMessage>,
    state: Arc<Mutex<OutboundMailboxState>>,
    wake_writer: Arc<Notify>,
    capacity_available: Arc<Notify>,
    shutdown: CancellationToken,
}

struct OutboundMailboxState {
    queue: VecDeque<OutboundMailboxEntry>,
    reliable_count: usize,
    bulk_count: usize,
    reliable_capacity: usize,
    bulk_capacity: usize,
    writer_in_flight: bool,
}

struct OutboundMailboxEntry {
    message: QueuedOutgoingMessage,
    class: ServerNotificationDeliveryClass,
}

enum MailboxEnqueueResult {
    Accepted,
    DroppedBestEffort,
    Full(QueuedOutgoingMessage),
    Closed(QueuedOutgoingMessage),
}

impl OutboundMailbox {
    fn new(writer: mpsc::Sender<QueuedOutgoingMessage>) -> Self {
        let writer_capacity = writer.max_capacity().max(1);
        let state = Arc::new(Mutex::new(OutboundMailboxState {
            queue: VecDeque::new(),
            reliable_count: 0,
            bulk_count: 0,
            reliable_capacity: writer_capacity,
            bulk_capacity: writer_capacity,
            writer_in_flight: false,
        }));
        let wake_writer = Arc::new(Notify::new());
        let capacity_available = Arc::new(Notify::new());
        let shutdown = CancellationToken::new();

        tokio::spawn(run_outbound_mailbox(
            writer.clone(),
            Arc::clone(&state),
            Arc::clone(&wake_writer),
            Arc::clone(&capacity_available),
            shutdown.clone(),
        ));

        Self {
            writer,
            state,
            wake_writer,
            capacity_available,
            shutdown,
        }
    }

    fn try_enqueue(&self, message: QueuedOutgoingMessage) -> MailboxEnqueueResult {
        if self.shutdown.is_cancelled() {
            return MailboxEnqueueResult::Closed(message);
        }

        let class = outgoing_delivery_class(&message.message);
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        if self.shutdown.is_cancelled() {
            return MailboxEnqueueResult::Closed(message);
        }

        if !state.writer_in_flight && state.queue.is_empty() {
            // Preserve the zero-buffer fast path and existing write-completion
            // semantics when the transport writer has immediate capacity.
            return match self.writer.try_send(message) {
                Ok(()) => MailboxEnqueueResult::Accepted,
                Err(mpsc::error::TrySendError::Full(message)) => {
                    self.enqueue_locked(&mut state, class, message)
                }
                Err(mpsc::error::TrySendError::Closed(message)) => {
                    self.shutdown.cancel();
                    MailboxEnqueueResult::Closed(message)
                }
            };
        }

        self.enqueue_locked(&mut state, class, message)
    }

    fn enqueue_locked(
        &self,
        state: &mut OutboundMailboxState,
        class: ServerNotificationDeliveryClass,
        mut message: QueuedOutgoingMessage,
    ) -> MailboxEnqueueResult {
        if class == ServerNotificationDeliveryClass::CoalescibleDelta
            && let Some(back) = state.queue.back_mut()
            && back.class == ServerNotificationDeliveryClass::CoalescibleDelta
        {
            match try_coalesce_queued_delta(&mut back.message, message) {
                Ok(()) => return MailboxEnqueueResult::Accepted,
                Err(returned) => message = returned,
            }
        }

        if class == ServerNotificationDeliveryClass::CoalescibleDelta
            && state.bulk_count >= state.bulk_capacity
            && let Some(index) = state.queue.iter().position(|entry| {
                entry.class == ServerNotificationDeliveryClass::BestEffort
            })
        {
            if let Some(removed) = state.queue.remove(index) {
                state.decrement(removed.class);
            }
        }

        let has_capacity = match class {
            ServerNotificationDeliveryClass::Reliable => {
                state.reliable_count < state.reliable_capacity
            }
            ServerNotificationDeliveryClass::CoalescibleDelta
            | ServerNotificationDeliveryClass::BestEffort => {
                state.bulk_count < state.bulk_capacity
            }
        };
        if !has_capacity {
            return if class == ServerNotificationDeliveryClass::BestEffort {
                MailboxEnqueueResult::DroppedBestEffort
            } else {
                MailboxEnqueueResult::Full(message)
            };
        }

        state.increment(class);
        state.queue.push_back(OutboundMailboxEntry { message, class });
        self.wake_writer.notify_one();
        MailboxEnqueueResult::Accepted
    }

    async fn enqueue_waiting_for_capacity(
        &self,
        mut message: QueuedOutgoingMessage,
    ) -> bool {
        loop {
            let notified = self.capacity_available.notified();
            match self.try_enqueue(message) {
                MailboxEnqueueResult::Accepted | MailboxEnqueueResult::DroppedBestEffort => {
                    return true;
                }
                MailboxEnqueueResult::Full(returned) => message = returned,
                MailboxEnqueueResult::Closed(_) => return false,
            }
            tokio::select! {
                _ = notified => {}
                _ = self.shutdown.cancelled() => return false,
            }
        }
    }

    fn close(&self) {
        self.shutdown.cancel();
        self.wake_writer.notify_waiters();
        self.capacity_available.notify_waiters();
    }
}

impl OutboundMailboxState {
    fn increment(&mut self, class: ServerNotificationDeliveryClass) {
        match class {
            ServerNotificationDeliveryClass::Reliable => self.reliable_count += 1,
            ServerNotificationDeliveryClass::CoalescibleDelta
            | ServerNotificationDeliveryClass::BestEffort => self.bulk_count += 1,
        }
    }

    fn decrement(&mut self, class: ServerNotificationDeliveryClass) {
        match class {
            ServerNotificationDeliveryClass::Reliable => self.reliable_count -= 1,
            ServerNotificationDeliveryClass::CoalescibleDelta
            | ServerNotificationDeliveryClass::BestEffort => self.bulk_count -= 1,
        }
    }
}

fn outgoing_delivery_class(message: &OutgoingMessage) -> ServerNotificationDeliveryClass {
    match message {
        OutgoingMessage::AppServerNotification(notification) => notification.delivery_class(),
        OutgoingMessage::Request(_) | OutgoingMessage::Response(_) | OutgoingMessage::Error(_) => {
            ServerNotificationDeliveryClass::Reliable
        }
    }
}

fn try_coalesce_queued_delta(
    current: &mut QueuedOutgoingMessage,
    newer: QueuedOutgoingMessage,
) -> Result<(), QueuedOutgoingMessage> {
    if current.write_complete_tx.is_some() || newer.write_complete_tx.is_some() {
        return Err(newer);
    }
    let QueuedOutgoingMessage {
        message: newer_message,
        write_complete_tx,
    } = newer;
    match (&mut current.message, newer_message) {
        (
            OutgoingMessage::AppServerNotification(current_notification),
            OutgoingMessage::AppServerNotification(newer_notification),
        ) => current_notification
            .try_coalesce_delta(newer_notification)
            .map_err(|notification| QueuedOutgoingMessage {
                message: OutgoingMessage::AppServerNotification(notification),
                write_complete_tx,
            }),
        (_, newer_message) => Err(QueuedOutgoingMessage {
            message: newer_message,
            write_complete_tx,
        }),
    }
}

async fn run_outbound_mailbox(
    writer: mpsc::Sender<QueuedOutgoingMessage>,
    state: Arc<Mutex<OutboundMailboxState>>,
    wake_writer: Arc<Notify>,
    capacity_available: Arc<Notify>,
    shutdown: CancellationToken,
) {
    loop {
        let next = {
            let mut state = state.lock().unwrap_or_else(|err| err.into_inner());
            match state.queue.pop_front() {
                Some(entry) => {
                    state.decrement(entry.class);
                    state.writer_in_flight = true;
                    capacity_available.notify_waiters();
                    Some(entry.message)
                }
                None => {
                    state.writer_in_flight = false;
                    None
                }
            }
        };

        if let Some(message) = next {
            let sent = tokio::select! {
                result = writer.send(message) => result.is_ok(),
                _ = shutdown.cancelled() => false,
            };
            if !sent {
                shutdown.cancel();
                capacity_available.notify_waiters();
                return;
            }
            continue;
        }

        tokio::select! {
            _ = wake_writer.notified() => {}
            _ = shutdown.cancelled() => return,
        }
    }
}

impl OutboundConnectionState {
    pub(crate) fn new(
        writer: mpsc::Sender<QueuedOutgoingMessage>,
        initialized: Arc<AtomicBool>,
        experimental_api_enabled: Arc<AtomicBool>,
        opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
        disconnect_sender: Option<CancellationToken>,
    ) -> Self {
        Self {
            initialized,
            experimental_api_enabled,
            opted_out_notification_methods,
            mailbox: OutboundMailbox::new(writer),
            disconnect_sender,
        }
    }

    fn can_disconnect(&self) -> bool {
        self.disconnect_sender.is_some()
    }

    pub(crate) fn request_disconnect(&self) {
        self.mailbox.close();
        if let Some(disconnect_sender) = &self.disconnect_sender {
            disconnect_sender.cancel();
        }
    }
}

impl Drop for OutboundConnectionState {
    fn drop(&mut self) {
        self.mailbox.close();
    }
}

fn should_skip_notification_for_connection(
    connection_state: &OutboundConnectionState,
    message: &OutgoingMessage,
) -> bool {
    let Ok(opted_out_notification_methods) = connection_state.opted_out_notification_methods.read()
    else {
        warn!("failed to read outbound opted-out notifications");
        return false;
    };
    match message {
        OutgoingMessage::AppServerNotification(notification) => {
            if notification.experimental_reason().is_some()
                && !connection_state
                    .experimental_api_enabled
                    .load(Ordering::Acquire)
            {
                return true;
            }
            let method = notification.to_string();
            opted_out_notification_methods.contains(method.as_str())
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
    write_complete_tx: Option<tokio::sync::oneshot::Sender<()>>,
    apply_notification_filter: bool,
) -> bool {
    let Some(connection_state) = connections.get(&connection_id) else {
        warn!("dropping message for disconnected connection: {connection_id:?}");
        return false;
    };
    let message = filter_outgoing_message_for_connection(connection_state, message);
    if apply_notification_filter
        && should_skip_notification_for_connection(connection_state, &message)
    {
        return false;
    }

    let mailbox = connection_state.mailbox.clone();
    let can_disconnect = connection_state.can_disconnect();
    let queued_message = QueuedOutgoingMessage {
        message,
        write_complete_tx,
    };
    match mailbox.try_enqueue(queued_message) {
        MailboxEnqueueResult::Accepted | MailboxEnqueueResult::DroppedBestEffort => false,
        MailboxEnqueueResult::Full(_queued_message) if can_disconnect => {
            warn!(
                "disconnecting slow connection after bounded outbound mailbox filled: {connection_id:?}"
            );
            disconnect_connection(connections, connection_id)
        }
        MailboxEnqueueResult::Full(queued_message) => {
            if mailbox.enqueue_waiting_for_capacity(queued_message).await {
                false
            } else {
                disconnect_connection(connections, connection_id)
            }
        }
        MailboxEnqueueResult::Closed(_queued_message) => {
            if can_disconnect {
                warn!(
                    "disconnecting connection after outbound mailbox closed: {connection_id:?}"
                );
            }
            disconnect_connection(connections, connection_id)
        }
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
    match envelope {
        OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx,
        } => {
            let _ = send_message_to_connection(
                connections,
                connection_id,
                message,
                write_complete_tx,
                /*apply_notification_filter*/ true,
            )
            .await;
        }
        OutgoingEnvelope::ToSnapshotAcceptedConnection {
            connection_id,
            message,
            write_complete_tx,
        } => {
            let _ = send_message_to_connection(
                connections,
                connection_id,
                message,
                write_complete_tx,
                /*apply_notification_filter*/ false,
            )
            .await;
        }
        OutgoingEnvelope::Broadcast { message } => {
            let target_connections: Vec<ConnectionId> = connections
                .iter()
                .filter_map(|(connection_id, connection_state)| {
                    if connection_state.initialized.load(Ordering::Acquire)
                        && !should_skip_notification_for_connection(connection_state, &message)
                    {
                        Some(*connection_id)
                    } else {
                        None
                    }
                })
                .collect();

            for connection_id in target_connections {
                let _ = send_message_to_connection(
                    connections,
                    connection_id,
                    message.clone(),
                    /*write_complete_tx*/ None,
                    /*apply_notification_filter*/ true,
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
