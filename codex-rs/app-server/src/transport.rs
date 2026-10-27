use crate::message_processor::ConnectionSessionState;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingWriteReceipt;
use codex_app_server_protocol::ExperimentalApi;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::JSONRPCRequest;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::Weak;
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
    pub(crate) request_ingress: Option<ConnectionRequestIngress>,
    pub(crate) disconnect: CancellationToken,
}

pub(crate) struct QueuedConnectionRequest {
    pub(crate) request: JSONRPCRequest,
    pub(crate) _credit: tokio::sync::OwnedSemaphorePermit,
}

pub(crate) struct ConnectionRequestIngress {
    sender: mpsc::Sender<QueuedConnectionRequest>,
    bytes: Arc<tokio::sync::Semaphore>,
}

impl ConnectionRequestIngress {
    pub(crate) fn channel() -> (Self, mpsc::Receiver<QueuedConnectionRequest>) {
        let (sender, receiver) = mpsc::channel(64);
        (Self { sender, bytes: Arc::new(tokio::sync::Semaphore::new(16 * 1024 * 1024)) }, receiver)
    }

    pub(crate) fn try_send(&self, request: JSONRPCRequest) -> bool {
        const LIMIT: usize = 16 * 1024 * 1024;
        let bytes = codex_app_server_transport::serialized_request_bytes_up_to(&request, LIMIT);
        if bytes > LIMIT {
            return false;
        }
        let Ok(credit) = Arc::clone(&self.bytes).try_acquire_many_owned(bytes.max(1) as u32) else {
            return false;
        };
        self.sender.try_send(QueuedConnectionRequest { request, _credit: credit }).is_ok()
    }
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
            request_ingress: None,
            disconnect: CancellationToken::new(),
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
    is_stdio: bool,
    stdio_delivery: Option<StdioDelivery>,
}

struct StdioDelivery {
    sender: mpsc::Sender<Arc<Mutex<Option<QueuedOutgoingMessage>>>>,
    tail: Weak<Mutex<Option<QueuedOutgoingMessage>>>,
}

const STDIO_DELIVERY_BYTES: usize = 16 * 1024 * 1024;
const STDIO_COALESCED_TEXT_BYTES: usize = 64 * 1024;

impl StdioDelivery {
    fn try_send(&mut self, message: QueuedOutgoingMessage) -> bool {
        // mpsc does not expose a mutable queued tail. The handoff node permits
        // adjacent text to append only until the existing consumer takes it.
        // No lock survives encoding, a capacity wait or a physical write.
        if let Some(tail) = self.tail.upgrade() {
            let Ok(mut queued) = tail.lock() else { return false; };
            if let Some(queued) = queued.as_mut()
                && coalesce_stdio_text(queued, &message)
            {
                return true;
            }
        }
        let queued = Arc::new(Mutex::new(Some(message)));
        if self.sender.try_send(Arc::clone(&queued)).is_err() {
            return false;
        }
        self.tail = Arc::downgrade(&queued);
        true
    }
}

fn coalesce_stdio_text(left: &mut QueuedOutgoingMessage, right: &QueuedOutgoingMessage) -> bool {
    // Receipts and all non-text/lifecycle messages remain exact wire boundaries.
    // Canonical publication and output recovery remain with upstream owners.
    if left.write_complete_tx.is_some() || right.write_complete_tx.is_some()
        || left.encoded_json.is_some() || right.encoded_json.is_some()
    {
        return false;
    }
    let (
        OutgoingMessage::AppServerNotification(left),
        OutgoingMessage::AppServerNotification(right),
    ) = (&mut left.message, &right.message) else { return false; };
    macro_rules! append {
        ($a:ident, $b:ident $(, $part:ident)?) => {{
            if $a.thread_id != $b.thread_id || $a.turn_id != $b.turn_id
                || $a.item_id != $b.item_id $(|| $a.$part != $b.$part)?
                || $b.delta.len() > STDIO_COALESCED_TEXT_BYTES.saturating_sub($a.delta.len())
            {
                false
            } else {
                $a.delta.push_str(&$b.delta);
                true
            }
        }};
    }
    use codex_app_server_protocol::ServerNotification;
    match (left, right) {
        (ServerNotification::AgentMessageDelta(a), ServerNotification::AgentMessageDelta(b)) => append!(a, b),
        (ServerNotification::PlanDelta(a), ServerNotification::PlanDelta(b)) => append!(a, b),
        (ServerNotification::ReasoningSummaryTextDelta(a), ServerNotification::ReasoningSummaryTextDelta(b)) => append!(a, b, summary_index),
        (ServerNotification::ReasoningTextDelta(a), ServerNotification::ReasoningTextDelta(b)) => append!(a, b, content_index),
        (ServerNotification::CommandExecutionOutputDelta(a), ServerNotification::CommandExecutionOutputDelta(b))
            if a.stream == b.stream && a.decoding_lossy == b.decoding_lossy => append!(a, b),
        _ => false,
    }
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
        let is_stdio = disconnect_sender.is_none();
        Self {
            initialized,
            experimental_api_enabled,
            opted_out_notification_methods,
            writer,
            disconnect_sender,
            transport_shutdown,
            is_stdio,
            stdio_delivery: None,
        }
    }

    pub(crate) fn with_stdio_delivery(mut self) -> Self {
        self.is_stdio = true;
        self
    }

    pub(crate) fn is_disconnected(&self) -> bool {
        self.disconnect_sender.as_ref().is_some_and(CancellationToken::is_cancelled)
            || self.transport_shutdown.is_cancelled()
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
    let queued_message = QueuedOutgoingMessage {
        message,
        encoded_json: None,
        write_complete_tx: write_complete_tx.map(|receipt| receipt.writer),
    };
    if !connection_state.is_stdio {
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
        // Stdio has an independent bounded delivery owner. Queue admission is
        // nonblocking here; encoding and byte-credit waits belong to that owner.
        let shutdown = connection_state.disconnect_sender.clone()
            .unwrap_or_else(|| connection_state.transport_shutdown.clone());
        if shutdown.is_cancelled() {
            return disconnect_connection(connections, connection_id);
        }
        let delivery = connection_state.stdio_delivery.get_or_insert_with(|| {
            // Preserve the existing 128-node limit. Coalesced text is at most
            // 64 KiB/node (8 MiB total), below the 16-MiB delivery-byte budget.
            // Individually larger legal frames still use the streaming owner.
            let (sender, mut receiver) =
                mpsc::channel::<Arc<Mutex<Option<QueuedOutgoingMessage>>>>(128);
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                let bytes = Arc::new(tokio::sync::Semaphore::new(STDIO_DELIVERY_BYTES));
                loop {
                    let next = tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => break,
                        next = receiver.recv() => next,
                    };
                    let Some(queued) = next else {
                        break;
                    };
                    let message = match queued.lock() {
                        Ok(mut queued) => queued.take(),
                        Err(_) => { shutdown.cancel(); break; }
                    };
                    let Some(mut message) = message else {
                        shutdown.cancel();
                        break;
                    };
                    let encoded_bytes = match message.prepare_stdio_json(STDIO_DELIVERY_BYTES) {
                        Ok(encoded_bytes) => encoded_bytes,
                        Err(error) => {
                            warn!(?connection_id, %error, "failed to encode bounded stdio output");
                            shutdown.cancel();
                            break;
                        }
                    };
                    if encoded_bytes > STDIO_DELIVERY_BYTES {
                        let (message_kind, request_id) = match &message.message {
                            OutgoingMessage::Response(response) => ("response", Some(&response.id)),
                            OutgoingMessage::Error(error) => ("error", Some(&error.id)),
                            OutgoingMessage::Request(_) => ("request", None),
                            OutgoingMessage::AppServerNotification(_) => ("notification", None),
                        };
                        warn!(
                            ?connection_id,
                            message_kind,
                            ?request_id,
                            encoded_bytes,
                            "streaming stdio output larger than the staging budget"
                        );
                    }
                    // The writer holds credit through physical write/flush. Wait
                    // here, not in the shared router, so a legal large frame can
                    // follow earlier frames and later frames remain ordered.
                    let credit = tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => break,
                        credit = Arc::clone(&bytes).acquire_many_owned(
                            encoded_bytes.clamp(1, STDIO_DELIVERY_BYTES) as u32,
                        ) => match credit {
                            Ok(credit) => credit,
                            Err(_) => { shutdown.cancel(); break; }
                        },
                    };
                    if let Some(encoded) = &mut message.encoded_json {
                        encoded.delivery_credit = Some(credit);
                    }
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => break,
                        result = writer.send(message) => {
                            if result.is_err() { shutdown.cancel(); break; }
                        }
                    }
                }
            });
            StdioDelivery { sender, tail: Weak::new() }
        });
        let sent = delivery.try_send(queued_message);
        if sent {
            return false;
        }
        // Queue saturation is a local, explicit delivery failure. Durable
        // thread state remains available to surviving/reconnecting clients.
        warn!(?connection_id, "retiring stdio connection after bounded output admission failed");
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

