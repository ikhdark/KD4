use super::CHANNEL_CAPACITY;
use super::TransportEvent;
use super::next_connection_id;
use super::protocol::ClientEnvelope;
pub use super::protocol::ClientEvent;
pub use super::protocol::ClientId;
use super::protocol::PongStatus;
use super::protocol::ServerEvent;
use super::protocol::StreamId;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::QueuedOutgoingMessage;
use crate::transport::ConnectionOrigin;
use crate::transport::remote_control::QueuedServerEnvelope;
use codex_app_server_protocol::JSONRPCMessage;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing::warn;

const REMOTE_CONTROL_CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub(crate) const REMOTE_CONTROL_IDLE_SWEEP_INTERVAL: Duration = Duration::from_secs(30);
const REMOTE_CONTROL_TRANSPORT_EVENT_SEND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(crate) struct Stopped;

struct ClientState {
    connection_id: ConnectionId,
    writer: mpsc::Sender<QueuedOutgoingMessage>,
    disconnect_token: CancellationToken,
    last_activity_at: Instant,
    last_inbound_seq_id: Option<u64>,
    // Keep accepted identities across same-stream replacement until retirement.
    // Count and serialized bytes share the existing ingress/message budgets.
    admitted_initializes: Vec<(u64, JSONRPCMessage, usize)>,
    status_tx: watch::Sender<PongStatus>,
}

pub(crate) struct ClientTracker {
    clients: HashMap<(ClientId, StreamId), ClientState>,
    legacy_stream_ids: HashMap<ClientId, StreamId>,
    join_set: JoinSet<(ClientId, StreamId, ConnectionId)>,
    cleanup: Vec<JoinHandle<Result<(), Stopped>>>,
    server_event_tx: mpsc::Sender<QueuedServerEnvelope>,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    // Unlike the child below, this is not cancelled by tracker-only shutdown.
    transport_shutdown_token: CancellationToken,
    shutdown_token: CancellationToken,
}

impl ClientTracker {
    pub(crate) fn new(
        server_event_tx: mpsc::Sender<QueuedServerEnvelope>,
        transport_event_tx: mpsc::Sender<TransportEvent>,
        shutdown_token: &CancellationToken,
    ) -> Self {
        Self {
            clients: HashMap::new(),
            legacy_stream_ids: HashMap::new(),
            join_set: JoinSet::new(),
            cleanup: Vec::new(),
            server_event_tx,
            transport_event_tx,
            transport_shutdown_token: shutdown_token.clone(),
            shutdown_token: shutdown_token.child_token(),
        }
    }

    pub(crate) async fn bookkeep_join_set(&mut self) -> Option<(ClientId, StreamId)> {
        while let Some(join_result) = self.join_set.join_next().await {
            let Ok((client_id, stream_id, connection_id)) = join_result else {
                continue;
            };
            let client_key = (client_id, stream_id);
            // Reinitialization may replace the same stream before its old writer exits.
            if self
                .clients
                .get(&client_key)
                .is_some_and(|client| client.connection_id == connection_id)
            {
                return Some(client_key);
            }
        }
        futures::future::pending().await
    }

    pub(crate) async fn shutdown(&mut self) {
        self.shutdown_token.cancel();
        self.close_all_clients().await;
    }

    pub(super) async fn close_all_clients(&mut self) {
        self.retire_clients();
        self.drain_join_set().await;
        let _ = self.wait_for_cleanup().await;
    }

    // Detach and cancel the entire old set before any reliable delivery can wait.
    // Cleanup tasks remain owned by the tracker across connection-worker aborts.
    pub(super) fn retire_clients(&mut self) {
        while let Some(client_key) = self.clients.keys().next().cloned() {
            if let Some(client) = self.remove_client(&client_key) {
                client.disconnect_token.cancel();
                let cleanup = self.spawn_connection_closed(client.connection_id);
                self.cleanup.push(cleanup);
            }
        }
    }

    async fn wait_for_cleanup(&mut self) -> Result<(), Stopped> {
        while let Some(cleanup) = self.cleanup.last_mut() {
            // Do not remove its owner before awaiting: cancellation must preserve it.
            let result = cleanup.await;
            self.cleanup.pop();
            result.map_err(|_| Stopped)??;
        }
        Ok(())
    }

    pub(super) fn contains_client_stream(
        &self,
        client_id: &ClientId,
        stream_id: Option<&StreamId>,
    ) -> bool {
        let Some(stream_id) = stream_id.or_else(|| self.legacy_stream_ids.get(client_id)) else {
            return false;
        };
        self.clients
            .contains_key(&(client_id.clone(), stream_id.clone()))
    }

    async fn drain_join_set(&mut self) {
        while self.join_set.join_next().await.is_some() {}
    }

    pub(crate) async fn handle_message(
        &mut self,
        client_envelope: ClientEnvelope,
    ) -> Result<(), Stopped> {
        // Account retirement and same-stream replacement cannot overtake closes.
        self.wait_for_cleanup().await?;
        let ClientEnvelope {
            client_id,
            event,
            stream_id,
            seq_id,
            cursor: _,
        } = client_envelope;
        let is_legacy_stream_id = stream_id.is_none();
        let is_initialize = matches!(&event, ClientEvent::ClientMessage { message } if remote_control_message_starts_connection(message));
        let stream_id = match stream_id {
            Some(stream_id) => stream_id,
            None if is_initialize => {
                // TODO(ruslan): delete this fallback once all clients are updated to send stream_id.
                self.legacy_stream_ids
                    .get(&client_id)
                    .cloned()
                    .unwrap_or_else(StreamId::new_random)
            }
            None => self
                .legacy_stream_ids
                .get(&client_id)
                .cloned()
                .unwrap_or_else(|| {
                    if matches!(&event, ClientEvent::Ping) {
                        StreamId::new_random()
                    } else {
                        StreamId(String::new())
                    }
                }),
        };
        if stream_id.0.is_empty() {
            return Ok(());
        }
        let client_key = (client_id.clone(), stream_id.clone());
        match event {
            ClientEvent::ClientMessage { message } => {
                // Only an exact admitted replay is redundant. A new initialize,
                // even with a reset sequence, still replaces the old generation.
                let admitted_initialize_size = if is_initialize && seq_id.is_some() {
                    let prior = self.clients.get(&client_key)
                        .map(|client| client.admitted_initializes.as_slice()).unwrap_or_default();
                    if prior.iter().any(|(admitted_seq_id, admitted, _)| {
                        Some(*admitted_seq_id) == seq_id && admitted == &message
                    }) {
                        return Ok(());
                    }
                    let size = serde_json::to_vec(&message).map_err(|error| {
                        warn!(%error, "failed to account initialize replay identity");
                        Stopped
                    })?.len();
                    let retained_bytes: usize = prior.iter().map(|(_, _, size)| *size).sum();
                    if prior.len() >= CHANNEL_CAPACITY
                        || size > super::segment::REMOTE_CONTROL_REASSEMBLED_MAX_BYTES
                            .saturating_sub(retained_bytes)
                    {
                        // Never evict an accepted identity and later replay its
                        // effects. A new stream can start a fresh bounded ledger.
                        warn!("remote control initialize replay identity capacity reached");
                        return Err(Stopped);
                    }
                    Some(size)
                } else {
                    None
                };
                if let Some(seq_id) = seq_id
                    && let Some(client) = self.clients.get(&client_key)
                    && client
                        .last_inbound_seq_id
                        .is_some_and(|last_seq_id| last_seq_id >= seq_id)
                    && !is_initialize
                {
                    return Ok(());
                }

                let mut admitted_initializes = Vec::new();
                if is_initialize && let Some(client) = self.clients.get_mut(&client_key) {
                    admitted_initializes = std::mem::take(&mut client.admitted_initializes);
                    self.close_client(&client_key).await?;
                }

                if let Some(connection_id) = self.clients.get_mut(&client_key).map(|client| {
                    client.last_activity_at = Instant::now();
                    client.connection_id
                }) {
                    self.send_transport_event(TransportEvent::IncomingMessage {
                        connection_id,
                        message,
                    })
                    .await?;
                    self.record_inbound_message_delivery(&client_key, seq_id);
                    return Ok(());
                }

                if !is_initialize {
                    return Ok(());
                }

                let connection_id = next_connection_id();
                let (writer_tx, writer_rx) =
                    mpsc::channel::<QueuedOutgoingMessage>(CHANNEL_CAPACITY);
                let disconnect_token = self.shutdown_token.child_token();
                self.send_transport_event(TransportEvent::ConnectionOpened {
                    connection_id,
                    origin: ConnectionOrigin::RemoteControl,
                    writer: writer_tx.clone(),
                    disconnect_sender: Some(disconnect_token.clone()),
                })
                .await?;

                let (status_tx, status_rx) = watch::channel(PongStatus::Active);
                self.join_set.spawn(Self::run_client_outbound(
                    client_id.clone(),
                    stream_id.clone(),
                    connection_id,
                    self.server_event_tx.clone(),
                    writer_rx,
                    status_rx,
                    disconnect_token.clone(),
                ));
                self.clients.insert(
                    client_key.clone(),
                    ClientState {
                        connection_id,
                        writer: writer_tx,
                        disconnect_token,
                        last_activity_at: Instant::now(),
                        last_inbound_seq_id: None,
                        admitted_initializes,
                        status_tx,
                    },
                );
                if is_legacy_stream_id {
                    self.legacy_stream_ids.insert(client_id.clone(), stream_id);
                }
                let admitted_initialize = seq_id.zip(admitted_initialize_size)
                    .map(|(seq_id, size)| (seq_id, message.clone(), size));
                if let Err(err) = self
                    .send_transport_event(TransportEvent::IncomingMessage {
                        connection_id,
                        message,
                    })
                    .await
                {
                    if let Some(client) = self.remove_client(&client_key) {
                        client.disconnect_token.cancel();
                        // The initialize send already timed out on this queue; preserve close
                        // delivery without blocking reconnect on the same backpressure.
                        let cleanup = self.spawn_connection_closed(client.connection_id);
                        self.cleanup.push(cleanup);
                    }
                    return Err(err);
                }
                if let Some(admitted_initialize) = admitted_initialize
                    && let Some(client) = self.clients.get_mut(&client_key)
                {
                    client.admitted_initializes.push(admitted_initialize);
                }
                if !is_legacy_stream_id {
                    self.record_inbound_message_delivery(&client_key, seq_id);
                }
                Ok(())
            }
            ClientEvent::ClientMessageChunk { .. } | ClientEvent::Ack { .. } => Ok(()),
            ClientEvent::Ping => {
                if let Some(client) = self.clients.get_mut(&client_key) {
                    client.last_activity_at = Instant::now();
                    let _ = client.status_tx.send(PongStatus::Active);
                    return Ok(());
                }

                // Unknown heartbeats are best effort. Waiting here would prevent the
                // reader from handling ACKs that free outbound capacity.
                let _ = self.server_event_tx.try_send(QueuedServerEnvelope {
                    event: ServerEvent::Pong {
                        status: PongStatus::Unknown,
                    },
                    client_id,
                    stream_id,
                    write_complete_tx: None,
                });
                Ok(())
            }
            ClientEvent::ClientClosed => self.close_client(&client_key).await,
        }
    }

    async fn run_client_outbound(
        client_id: ClientId,
        stream_id: StreamId,
        connection_id: ConnectionId,
        server_event_tx: mpsc::Sender<QueuedServerEnvelope>,
        mut writer_rx: mpsc::Receiver<QueuedOutgoingMessage>,
        mut status_rx: watch::Receiver<PongStatus>,
        disconnect_token: CancellationToken,
    ) -> (ClientId, StreamId, ConnectionId) {
        loop {
            let (event, write_complete_tx) = tokio::select! {
                _ = disconnect_token.cancelled() => {
                    break;
                }
                queued_message = writer_rx.recv() => {
                    let Some(queued_message) = queued_message else {
                        break;
                    };
                    let event = ServerEvent::ServerMessage {
                        message: Box::new(queued_message.message),
                    };
                    (event, queued_message.write_complete_tx)
                }
                changed = status_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let event = ServerEvent::Pong { status: status_rx.borrow().clone() };
                    (event, None)
                }
            };
            let send_result = tokio::select! {
                _ = disconnect_token.cancelled() => {
                    break;
                }
                send_result = server_event_tx.send(QueuedServerEnvelope {
                    event,
                    client_id: client_id.clone(),
                    stream_id: stream_id.clone(),
                    write_complete_tx,
                }) => send_result,
            };
            if send_result.is_err() {
                break;
            }
        }
        (client_id, stream_id, connection_id)
    }

    pub(crate) async fn close_expired_clients(
        &mut self,
    ) -> Result<Vec<(ClientId, StreamId)>, Stopped> {
        let expired_client_ids = self.expired_client_keys();
        for client_key in &expired_client_ids {
            self.close_client(client_key).await?;
        }
        Ok(expired_client_ids)
    }

    pub(super) fn expired_client_keys(&self) -> Vec<(ClientId, StreamId)> {
        let now = Instant::now();
        self
            .clients
            .iter()
            .filter_map(|(client_key, client)| {
                (!remote_control_client_is_alive(client, now)).then_some(client_key.clone())
            })
            .collect()
    }

    pub(super) fn record_ack_activity(
        &mut self,
        client_key: &(ClientId, StreamId),
        activity_at: Instant,
    ) {
        if let Some(client) = self.clients.get_mut(client_key) {
            client.last_activity_at = client.last_activity_at.max(activity_at);
        }
    }

    pub(super) async fn close_client(
        &mut self,
        client_key: &(ClientId, StreamId),
    ) -> Result<(), Stopped> {
        let Some(client) = self.remove_client(client_key) else {
            return Ok(());
        };
        client.disconnect_token.cancel();
        let cleanup = self.spawn_connection_closed(client.connection_id);
        self.cleanup.push(cleanup);
        self.wait_for_cleanup().await
    }

    fn remove_client(&mut self, client_key: &(ClientId, StreamId)) -> Option<ClientState> {
        let client = self.clients.remove(client_key)?;
        if self
            .legacy_stream_ids
            .get(&client_key.0)
            .is_some_and(|stream_id| stream_id == &client_key.1)
        {
            self.legacy_stream_ids.remove(&client_key.0);
        }
        Some(client)
    }

    async fn send_transport_event(&self, event: TransportEvent) -> Result<(), Stopped> {
        let event = match event {
            TransportEvent::ConnectionClosed { connection_id } => {
                return self.send_connection_closed(connection_id).await;
            }
            event => event,
        };

        if let TransportEvent::IncomingMessage {
            connection_id,
            message: JSONRPCMessage::Request(_),
            ..
        } = &event
        {
            let Some(client) = self.clients.values().find(|client| client.connection_id == *connection_id) else {
                return Err(Stopped);
            };
            let TransportEvent::IncomingMessage { connection_id, message } = event else { unreachable!() };
            return if crate::transport::enqueue_incoming_message(&self.transport_event_tx, &client.writer,
                connection_id, message).await { Ok(()) } else { Err(Stopped) };
        }
        let event_name = transport_event_name(&event);
        match timeout(
            REMOTE_CONTROL_TRANSPORT_EVENT_SEND_TIMEOUT,
            self.transport_event_tx.send(event),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => {
                warn!(
                    transport_event = event_name,
                    "remote control transport event receiver dropped"
                );
                Err(Stopped)
            }
            Err(_) => {
                warn!(
                    transport_event = event_name,
                    timeout = ?REMOTE_CONTROL_TRANSPORT_EVENT_SEND_TIMEOUT,
                    "timed out forwarding remote control transport event"
                );
                Err(Stopped)
            }
        }
    }

    fn record_inbound_message_delivery(
        &mut self,
        client_key: &(ClientId, StreamId),
        seq_id: Option<u64>,
    ) {
        // Timed forwarding can fail, so only dedupe retries after app-server receives it.
        if let Some(seq_id) = seq_id
            && let Some(client) = self.clients.get_mut(client_key)
        {
            client.last_inbound_seq_id = Some(seq_id);
        }
    }

    async fn send_connection_closed(&self, connection_id: ConnectionId) -> Result<(), Stopped> {
        // Worker shutdown can abort the caller; detach the cleanup event before awaiting it.
        match self.spawn_connection_closed(connection_id).await {
            Ok(result) => result,
            Err(err) => {
                warn!(
                    transport_event = "connection_closed",
                    ?err,
                    "remote control transport event forwarding task failed"
                );
                Err(Stopped)
            }
        }
    }

    fn spawn_connection_closed(
        &self,
        connection_id: ConnectionId,
    ) -> JoinHandle<Result<(), Stopped>> {
        info!(
            connection_id = ?connection_id,
            "forwarding remote control connection closed transport event"
        );
        let transport_event_tx = self.transport_event_tx.clone();
        let transport_shutdown_token = self.transport_shutdown_token.clone();
        tokio::spawn(async move {
            let event = TransportEvent::ConnectionClosed { connection_id };
            let result = match transport_event_tx.try_send(event) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(event)) => {
                    // Preserve cleanup across caller cancellation and reconnects, but
                    // do not retain a sender after the transport itself shuts down.
                    tokio::select! {
                        biased;
                        _ = transport_shutdown_token.cancelled() => return Ok(()),
                        result = transport_event_tx.send(event) => result,
                    }
                }
                Err(mpsc::error::TrySendError::Closed(event)) => Err(mpsc::error::SendError(event)),
            };
            result.map_err(|_| {
                warn!(
                    transport_event = "connection_closed",
                    "remote control transport event receiver dropped"
                );
                Stopped
            })
        })
    }
}

fn transport_event_name(event: &TransportEvent) -> &'static str {
    match event {
        TransportEvent::ConnectionOpened { .. } => "connection_opened",
        TransportEvent::ConnectionClosed { .. } => "connection_closed",
        TransportEvent::IncomingMessage { .. } => "incoming_message",
    }
}

fn remote_control_message_starts_connection(message: &JSONRPCMessage) -> bool {
    matches!(
        message,
        JSONRPCMessage::Request(codex_app_server_protocol::JSONRPCRequest { method, .. })
            if method == "initialize"
    )
}

fn remote_control_client_is_alive(client: &ClientState, now: Instant) -> bool {
    now.duration_since(client.last_activity_at) < REMOTE_CONTROL_CLIENT_IDLE_TIMEOUT
}

