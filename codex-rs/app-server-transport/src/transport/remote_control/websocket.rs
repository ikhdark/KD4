use super::CurrentRemoteControlEnrollment;
use super::REMOTE_CONTROL_INSTALLATION_ID_HEADER;
use super::RemoteControlEnrollmentSelection;
use super::RemoteControlPairingPersistenceKey;
use super::RemoteControlPolicyDecision;
use super::desired_state::RemoteControlDesiredState;
use super::desired_state::acquire_persistence_lock;
use super::desired_state::desired_state_from_persisted_enrollment;
use super::policy_admits_current_identity;
use super::protocol::ClientEnvelope;
use super::protocol::ClientEvent;
use super::protocol::ClientId;
use super::protocol::RemoteControlTarget;
use super::protocol::ServerEnvelope;
use super::protocol::StreamId;
use super::protocol::remote_control_user_agent;
use super::remote_control_status_with_connection_status;
use super::same_remote_control_enrollment;
use super::segment::ClientSegmentObservation;
use super::segment::ClientSegmentReassembler;
use super::segment::EncodedServerPayload;
use super::segment::REMOTE_CONTROL_SEGMENT_MAX_BYTES;
use super::segment::encode_server_payloads_for_transport;
use crate::transport::TransportEvent;
use crate::transport::remote_control::auth::RemoteControlConnectionAuth;
use crate::transport::remote_control::auth::load_remote_control_auth;
use crate::transport::remote_control::auth::recover_remote_control_auth;
use crate::transport::remote_control::client_tracker::ClientTracker;
use crate::transport::remote_control::client_tracker::REMOTE_CONTROL_IDLE_SWEEP_INTERVAL;
use crate::transport::remote_control::enroll::RemoteControlEnrollment;
use crate::transport::remote_control::enroll::format_headers;
use crate::transport::remote_control::enroll::load_persisted_remote_control_enrollment;
use crate::transport::remote_control::enroll::preview_remote_control_response_body;
use crate::transport::remote_control::enroll::update_persisted_remote_control_enrollment;
use crate::transport::remote_control::server_api::enroll_remote_control_server_with_client;
use crate::transport::remote_control::server_api::refresh_remote_control_server_with_client;
use axum::http::HeaderValue;
use base64::Engine;
use codex_app_server_protocol::RemoteControlConnectionStatus;
use codex_app_server_protocol::RemoteControlStatusChangedNotification;
use codex_client::capped_backoff;
use codex_http_client::RouteAwareClientPool;
use codex_http_client::maybe_build_rustls_client_config_with_custom_ca;
use codex_login::AuthManager;
use codex_login::UnauthorizedRecovery;
use codex_login::default_client::default_headers;
use codex_state::StateRuntime;
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;
use futures::SinkExt;
use futures::StreamExt;
use futures::stream::SplitSink;
use futures::stream::SplitStream;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io;
use std::io::ErrorKind;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::Connector;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use tracing::info;
use tracing::warn;

pub(super) const REMOTE_CONTROL_PROTOCOL_VERSION: &str = "3";
const REMOTE_CONTROL_SUBSCRIBE_CURSOR_HEADER: &str = "x-codex-subscribe-cursor";
const REMOTE_CONTROL_WEBSOCKET_PING_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(10);
const REMOTE_CONTROL_WEBSOCKET_PONG_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(60);
const REMOTE_CONTROL_ACCOUNT_ID_RETRY_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(1);
const REMOTE_CONTROL_RECONNECT_BACKOFF_CAP: std::time::Duration =
    std::time::Duration::from_secs(30);
const REMOTE_CONTROL_WEBSOCKET_CONNECT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);
const REMOTE_CONTROL_CONNECTION_SHUTDOWN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);
const REMOTE_CONTROL_REPLAY_BATCH_MESSAGES: usize = 64;
const REMOTE_CONTROL_REPLAY_BATCH_TARGET_BYTES: usize = 64 * 1024;
const REMOTE_APP_SERVER_NOT_FOUND_DETAIL: &str = "Remote app server not found";


struct BufferedServerEnvelope {
    client_id: ClientId,
    stream_id: StreamId,
    seq_id: u64,
    segment_id: Option<usize>,
    payload: tungstenite::Utf8Bytes,
}

impl BufferedServerEnvelope {


    fn from_payload(encoded: EncodedServerPayload) -> Self {
        Self {
            client_id: encoded.client_id,
            stream_id: encoded.stream_id,
            seq_id: encoded.seq_id,
            segment_id: encoded.segment_id,
            payload: encoded.payload.into(),
        }
    }
}


struct ScheduledServerFrame {
    payload: tungstenite::Utf8Bytes,
    completion: Option<oneshot::Sender<std::time::Instant>>,
}

// Sequence tombstones protect reconnect replay until the transport generation
// retires. Bound their lifetime by rotating the generation, never by forgetting
// an individual sequence while queued/replayed output can still name it.
const MAX_TRANSPORT_STREAM_IDENTITIES: usize = 4096;

#[derive(Default)]
struct ServerWriterQueue {
    streams: HashMap<(ClientId, StreamId), VecDeque<ScheduledServerFrame>>,
    ready: VecDeque<(ClientId, StreamId)>,
}

impl ServerWriterQueue {
    fn is_empty(&self) -> bool { self.ready.is_empty() }

    fn push(&mut self, key: (ClientId, StreamId), payload: tungstenite::Utf8Bytes,
        completion: Option<oneshot::Sender<std::time::Instant>>) {
        let stream = self.streams.entry(key.clone()).or_default();
        if stream.is_empty() { self.ready.push_back(key); }
        stream.push_back(ScheduledServerFrame { payload, completion });
    }

    fn pop(&mut self) -> Option<ScheduledServerFrame> {
        let key = self.ready.pop_front()?;
        let stream = self.streams.get_mut(&key)?;
        let frame = stream.pop_front();
        if stream.is_empty() { self.streams.remove(&key); }
        else { self.ready.push_back(key); }
        frame
    }

    async fn enqueue(&mut self, state: &Mutex<WebsocketState>, queued: super::QueuedServerEnvelope) -> io::Result<((ClientId, StreamId), u64)> {
        let key = (queued.client_id.clone(), queued.stream_id.clone());
        let seq_id = {
            let state = state.lock().await;
            if state.next_seq_id_by_stream.len() >= MAX_TRANSPORT_STREAM_IDENTITIES
                && !state.next_seq_id_by_stream.contains_key(&key)
            {
                return Err(io::Error::new(ErrorKind::WouldBlock, "remote control stream identity capacity reached; retiring transport generation"));
            }
            *state.next_seq_id_by_stream.get(&key).unwrap_or(&1)
        };
        let envelope = ServerEnvelope {
            event: queued.event, client_id: queued.client_id, stream_id: queued.stream_id, seq_id,
        };
        // Complete encoding before publishing any sequence/replay mutation.
        let encoded = encode_server_payloads_for_transport(envelope)?;
        let count = encoded.len();
        let mut completion = queued.write_complete_tx;
        let mut state = state.lock().await;
        for (index, encoded) in encoded.into_iter().enumerate() {
            let buffered = BufferedServerEnvelope::from_payload(encoded);
            self.push(key.clone(), buffered.payload.clone(),
                if index + 1 == count { completion.take() } else { None });
            state.outbound_buffer.insert_encoded(buffered);
        }
        state.next_seq_id_by_stream.insert(key.clone(), seq_id.saturating_add(1));
        Ok((key, seq_id))
    }
}

// A blocked individual sink operation cannot be interleaved safely. Retire the
// connection at the same liveness bound rather than cancel/restart a partial frame.
async fn writer_operation(
    shutdown: &CancellationToken,
    operation: impl std::future::Future<Output = Result<(), tungstenite::Error>>,
) -> io::Result<()> {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => Ok(()),
        result = tokio::time::timeout(REMOTE_CONTROL_WEBSOCKET_PONG_TIMEOUT, operation) => {
            result.map_err(|_| io::Error::new(ErrorKind::TimedOut, "remote control websocket write stalled"))?
                .map_err(io::Error::other)
        }
    }
}

struct BoundedOutboundBuffer {
    buffer_by_stream: HashMap<(ClientId, StreamId), VecDeque<BufferedServerEnvelope>>,
    used_tx: watch::Sender<usize>,
}

impl BoundedOutboundBuffer {
    fn new() -> (Self, watch::Receiver<usize>) {
        let (used_tx, used_rx) = watch::channel(0);
        let buffer = Self {
            buffer_by_stream: HashMap::new(),
            used_tx,
        };
        (buffer, used_rx)
    }


    fn insert_encoded(&mut self, server_envelope: BufferedServerEnvelope) {
        self.buffer_by_stream
            .entry((
                server_envelope.client_id.clone(),
                server_envelope.stream_id.clone(),
            ))
            .or_default()
            .push_back(server_envelope);
        self.used_tx.send_modify(|used| *used += 1);
    }

    fn ack(
        &mut self,
        client_id: &ClientId,
        stream_id: &StreamId,
        acked_seq_id: u64,
        acked_segment_id: Option<usize>,
    ) -> bool {
        let key = (client_id.clone(), stream_id.clone());
        let Some(buffer) = self.buffer_by_stream.get_mut(&key) else {
            return false;
        };
        let acked_cursor = (acked_seq_id, acked_segment_id.unwrap_or(usize::MAX));
        // The single writer inserts sequence IDs and their segments in order.
        let mut removed = 0;
        while buffer.front().is_some_and(|envelope| {
            (envelope.seq_id, envelope.segment_id.unwrap_or_default()) <= acked_cursor
        }) {
            buffer.pop_front();
            removed += 1;
        }
        if removed != 0 {
            self.used_tx.send_modify(|used| *used -= removed);
        }
        if buffer.is_empty() {
            self.buffer_by_stream.remove(&key);
        }
        removed != 0
    }

    fn server_envelopes(&self) -> impl Iterator<Item = &BufferedServerEnvelope> {
        self.buffer_by_stream
            .values()
            .flat_map(|buffer| buffer.iter())
    }
}

struct ClientDeliveryMetadata {
    cursor: Option<String>,
    ack: Option<(ClientId, StreamId, u64, Option<usize>)>,
}

impl From<&ClientEnvelope> for ClientDeliveryMetadata {
    fn from(envelope: &ClientEnvelope) -> Self {
        let ack = match (&envelope.event, envelope.seq_id, &envelope.stream_id) {
            (ClientEvent::Ack { segment_id }, Some(seq_id), Some(stream_id)) => Some((
                envelope.client_id.clone(),
                stream_id.clone(),
                seq_id,
                *segment_id,
            )),
            _ => None,
        };
        Self {
            cursor: envelope.cursor.clone(),
            ack,
        }
    }
}


type ClientIngressWork = (
    ClientEnvelope,
    oneshot::Sender<Result<bool, super::client_tracker::Stopped>>,
);

struct PendingClientDelivery {
    delivery: ClientDeliveryMetadata,
    client_message_key: Option<((ClientId, Option<StreamId>), u64)>,
    ordinal: usize,
    completion: oneshot::Receiver<Result<bool, super::client_tracker::Stopped>>,
    bytes: usize,
}

impl PendingClientDelivery {
    fn commit(self, state: &mut WebsocketState, delivered_to_stream: bool) {
        state.record_client_message_delivery(self.delivery, self.client_message_key.filter(|_| delivered_to_stream));
    }
}

#[expect(clippy::await_holding_invalid_type, reason = "one ordered ingress owner serializes client lifecycle")]
async fn run_client_ingress(
    tracker: Arc<Mutex<ClientTracker>>,
    state: Arc<Mutex<WebsocketState>>,
    mut ingress: mpsc::Receiver<ClientIngressWork>,
    shutdown: CancellationToken,
) {
    let mut tracker = tracker.lock().await;
    let mut idle_sweep = tokio::time::interval(REMOTE_CONTROL_IDLE_SWEEP_INTERVAL);
    idle_sweep.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        let work = tokio::select! {
            _ = shutdown.cancelled() => return,
            key = tracker.bookkeep_join_set() => {
                if let Some((client, stream)) = key {
                    {
                        let mut state = state.lock().await;
                        state.client_segment_reassembler.invalidate_stream(&client, &stream);
                        state.invalidate_client_message_stream(&client, &stream);
                    }
                    if tracker.close_client(&(client.clone(), stream.clone())).await.is_err() { return; }
                }
                continue;
            }
            _ = idle_sweep.tick() => {
                // The reader must never wait for the tracker lock to release
                // ACK capacity while application forwarding is backpressured.
                let keys = {
                    let mut state = state.lock().await;
                    // Linearize ACK publication and the expiry decision. An ACK
                    // accepted before this cutoff must participate in liveness.
                    let activity = std::mem::take(&mut state.last_ack_activity_by_stream);
                    for (key, activity_at) in activity {
                        tracker.record_ack_activity(&key, activity_at);
                    }
                    let keys = tracker.expired_client_keys();
                    for (client, stream) in &keys {
                        state.client_segment_reassembler.invalidate_stream(client, stream);
                        state.invalidate_client_message_stream(client, stream);
                    }
                    keys
                };
                for key in keys {
                    if tracker.close_client(&key).await.is_err() { return; }
                }
                continue;
            }
            work = ingress.recv() => {
                let Some(work) = work else { return };
                work
            }
        };
        let (envelope, completion) = work;
        let key = (envelope.client_id.clone(), envelope.stream_id.clone());
        let result = tracker.handle_message(envelope).await
            .map(|()| tracker.contains_client_stream(&key.0, key.1.as_ref()));
        let failed = result.is_err();
        let _ = completion.send(result);
        if failed { return; }
    }
}

struct WebsocketState {
    outbound_buffer: BoundedOutboundBuffer,
    subscribe_cursor: Option<String>,
    next_seq_id_by_stream: HashMap<(ClientId, StreamId), u64>,
    last_ack_activity_by_stream: HashMap<(ClientId, StreamId), tokio::time::Instant>,
    last_completed_client_chunk_seq_id_by_stream: HashMap<(ClientId, Option<StreamId>), u64>,
    client_segment_reassembler: ClientSegmentReassembler,
}

impl WebsocketState {
    fn observe_client_message(
        &mut self,
        client_envelope: ClientEnvelope,
        wire_size_bytes: usize,
    ) -> ClientSegmentObservation {
        let client_message_key = Self::client_message_key(&client_envelope);
        if let Some((key, seq_id)) = client_message_key.as_ref()
            && self
                .last_completed_client_chunk_seq_id_by_stream
                .get(key)
                .is_some_and(|last_seq_id| last_seq_id >= seq_id)
        {
            return ClientSegmentObservation::Dropped;
        }
        if let (
            Some((_, seq_id)),
            Some(stream_id),
            ClientEvent::ClientMessageChunk { segment_id, .. },
        ) = (
            client_message_key.as_ref(),
            client_envelope.stream_id.as_ref(),
            &client_envelope.event,
        ) && self.client_segment_reassembler.should_ignore_chunk(
            &client_envelope.client_id,
            stream_id,
            *seq_id,
            *segment_id,
        ) {
            return ClientSegmentObservation::Dropped;
        }
        if client_message_key.is_some() && wire_size_bytes > REMOTE_CONTROL_SEGMENT_MAX_BYTES {
            warn!(
                client_id = client_envelope.client_id.0.as_str(),
                "dropping oversized segmented remote-control client envelope"
            );
            if let Some(stream_id) = client_envelope.stream_id.as_ref() {
                self.client_segment_reassembler
                    .invalidate_stream(&client_envelope.client_id, stream_id);
            }
            return ClientSegmentObservation::Dropped;
        }

        self.client_segment_reassembler.observe(client_envelope)
    }

    fn record_client_message_delivery(
        &mut self,
        delivery: ClientDeliveryMetadata,
        client_message_key: Option<((ClientId, Option<StreamId>), u64)>,
    ) {
        if let Some(cursor) = delivery.cursor {
            self.subscribe_cursor = Some(cursor);
        }
        if let Some((key, seq_id)) = client_message_key {
            self.last_completed_client_chunk_seq_id_by_stream
                .insert(key, seq_id);
        }
        if let Some((client_id, stream_id, seq_id, segment_id)) = delivery.ack {
            self.record_ack(&client_id, &stream_id, seq_id, segment_id);
        }
    }

    fn record_ack(&mut self, client_id: &ClientId, stream_id: &StreamId, seq_id: u64, segment_id: Option<usize>) {
        if self.outbound_buffer.ack(client_id, stream_id, seq_id, segment_id) {
            // Confirmed consumption is activity; merely generating or buffering
            // output is not proof that this virtual subscriber is alive.
            self.last_ack_activity_by_stream.insert(
                (client_id.clone(), stream_id.clone()), tokio::time::Instant::now(),
            );
        }
    }

    fn invalidate_client_message_stream(&mut self, client_id: &ClientId, stream_id: &StreamId) {
        self.last_ack_activity_by_stream.remove(&(client_id.clone(), stream_id.clone()));
        self.last_completed_client_chunk_seq_id_by_stream
            .remove(&(client_id.clone(), Some(stream_id.clone())));
    }

    fn invalidate_client_message_client(&mut self, client_id: &ClientId) {
        self.last_ack_activity_by_stream.retain(|(client, _), _| client != client_id);
        self.last_completed_client_chunk_seq_id_by_stream
            .retain(|(cursor_client_id, _), _| cursor_client_id != client_id);
    }

    fn client_message_key(
        client_envelope: &ClientEnvelope,
    ) -> Option<((ClientId, Option<StreamId>), u64)> {
        let seq_id = match (&client_envelope.event, client_envelope.seq_id) {
            (ClientEvent::ClientMessageChunk { .. }, Some(seq_id)) => seq_id,
            _ => return None,
        };
        Some((
            (
                client_envelope.client_id.clone(),
                client_envelope.stream_id.clone(),
            ),
            seq_id,
        ))
    }
}

pub(crate) struct RemoteControlWebsocket {
    remote_control_url: String,
    installation_id: String,
    server_name: String,
    remote_control_target: Option<RemoteControlTarget>,
    state_db: Option<Arc<StateRuntime>>,
    auth_manager: Arc<AuthManager>,
    status_publisher: RemoteControlStatusPublisher,
    shutdown_token: CancellationToken,
    reconnect_attempt: u64,
    auth_recovery: UnauthorizedRecovery,
    auth_change_rx: watch::Receiver<u64>,
    current_enrollment: CurrentRemoteControlEnrollment,
    transport_enrollment: Option<RemoteControlEnrollment>,
    pairing_persistence_key: RemoteControlPairingPersistenceKey,
    client_tracker: Arc<Mutex<ClientTracker>>,
    state: Arc<Mutex<WebsocketState>>,
    server_event_rx: Arc<Mutex<mpsc::Receiver<super::QueuedServerEnvelope>>>,
    used_rx: watch::Receiver<usize>,
    desired_state_tx: Arc<watch::Sender<RemoteControlDesiredState>>,
    desired_state_rx: watch::Receiver<RemoteControlDesiredState>,
    desired_state_persistence_lock: Arc<Semaphore>,
    http_clients: RouteAwareClientPool,
    policy_rx: Option<watch::Receiver<RemoteControlPolicyDecision>>,
}

pub(crate) struct RemoteControlWebsocketConfig {
    pub(crate) remote_control_url: String,
    pub(crate) installation_id: String,
    pub(crate) remote_control_target: Option<RemoteControlTarget>,
    pub(crate) server_name: String,
    pub(crate) http_clients: RouteAwareClientPool,
}

pub(super) struct RemoteControlAuthContext<'a> {
    auth_manager: &'a Arc<AuthManager>,
    auth_recovery: &'a mut UnauthorizedRecovery,
    auth_change_rx: &'a mut watch::Receiver<u64>,
}

struct RemoteControlEnrollmentAuthContext<'a, 'b> {
    auth: &'a RemoteControlConnectionAuth,
    recovery: &'a mut RemoteControlAuthContext<'b>,
}

enum ConnectOutcome {
    Connected(Box<WebSocketStream<MaybeTlsStream<TcpStream>>>),
    Disabled,
    Shutdown,
}

#[derive(Debug)]
struct RemoteControlStateRepaired(String);

impl std::fmt::Display for RemoteControlStateRepaired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RemoteControlStateRepaired {}

fn state_repaired_error(message: String) -> io::Error {
    io::Error::other(RemoteControlStateRepaired(message))
}

fn take_repaired_retry(err: &io::Error, available: &mut bool) -> bool {
    if *available && err.get_ref().is_some_and(|err| err.is::<RemoteControlStateRepaired>()) {
        *available = false;
        return true;
    }
    false
}

#[derive(Debug, Clone, Copy)]
enum ConnectionEndReason {
    Shutdown,
    Disabled,
    EnabledWatchClosed,
    AuthChanged,
    ConnectionWorkerStopped,
}

pub(super) struct RemoteControlChannels {
    pub(super) transport_event_tx: mpsc::Sender<TransportEvent>,
    pub(super) status_publisher: RemoteControlStatusPublisher,
    pub(super) current_enrollment: CurrentRemoteControlEnrollment,
    pub(super) pairing_persistence_key: RemoteControlPairingPersistenceKey,
    pub(super) desired_state_persistence_lock: Arc<Semaphore>,
}

#[derive(Clone)]
pub(super) struct RemoteControlStatusPublisher {
    tx: watch::Sender<RemoteControlStatusChangedNotification>,
}

impl RemoteControlStatusPublisher {
    pub(super) fn new(tx: watch::Sender<RemoteControlStatusChangedNotification>) -> Self {
        Self { tx }
    }

    fn status(&self) -> RemoteControlStatusChangedNotification {
        self.tx.borrow().clone()
    }

    fn publish_status(&self, connection_status: RemoteControlConnectionStatus) {
        let mut status_change = None;
        self.tx.send_if_modified(|status| {
            let next_status =
                remote_control_status_with_connection_status(status, connection_status);
            if *status == next_status {
                return false;
            }

            status_change = Some((status.clone(), next_status.clone()));
            *status = next_status;
            true
        });
        if let Some((previous_status, next_status)) = status_change {
            info!(
                previous_status = ?previous_status.status,
                next_status = ?next_status.status,
                previous_environment_id = ?previous_status.environment_id,
                next_environment_id = ?next_status.environment_id,
                installation_id = %next_status.installation_id,
                server_name = %next_status.server_name,
                "remote control websocket status changed"
            );
        }
    }

    pub(super) fn publish_environment_id(&self, environment_id: Option<String>) {
        let mut status_change = None;
        self.tx.send_if_modified(|status| {
            if status.status == RemoteControlConnectionStatus::Disabled {
                return false;
            }
            let next_status = RemoteControlStatusChangedNotification {
                status: status.status,
                server_name: status.server_name.clone(),
                installation_id: status.installation_id.clone(),
                environment_id,
            };
            if *status == next_status {
                return false;
            }

            status_change = Some((status.clone(), next_status.clone()));
            *status = next_status;
            true
        });
        if let Some((previous_status, next_status)) = status_change {
            info!(
                status = ?next_status.status,
                previous_environment_id = ?previous_status.environment_id,
                next_environment_id = ?next_status.environment_id,
                installation_id = %next_status.installation_id,
                server_name = %next_status.server_name,
                "remote control websocket environment changed"
            );
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct RemoteControlConnectOptions<'a> {
    installation_id: &'a str,
    server_name: &'a str,
    subscribe_cursor: Option<(&'a RemoteControlEnrollment, &'a str)>,
    app_server_client_name: Option<&'a str>,
    desired_state_tx: &'a watch::Sender<RemoteControlDesiredState>,
    desired_state_persistence_lock: &'a Semaphore,
    http_clients: &'a RouteAwareClientPool,
    request_state: &'a super::RemoteControlEnrollmentState,
    /// Managed-requirements decision that must admit the current identity before any backend
    /// contact for its account.
    policy: Option<&'a watch::Receiver<RemoteControlPolicyDecision>>,
}

impl RemoteControlWebsocket {
    pub(crate) fn new(
        config: RemoteControlWebsocketConfig,
        state_db: Option<Arc<StateRuntime>>,
        auth_manager: Arc<AuthManager>,
        channels: RemoteControlChannels,
        shutdown_token: CancellationToken,
        desired_state_tx: Arc<watch::Sender<RemoteControlDesiredState>>,
    ) -> Self {
        let shutdown_token = shutdown_token.child_token();
        let (server_event_tx, server_event_rx) = mpsc::channel(super::CHANNEL_CAPACITY);
        let client_tracker = ClientTracker::new(
            server_event_tx,
            channels.transport_event_tx,
            &shutdown_token,
        );
        let (outbound_buffer, used_rx) = BoundedOutboundBuffer::new();
        let auth_recovery = auth_manager.unauthorized_recovery();
        let auth_change_rx = auth_manager.auth_change_receiver();

        let desired_state_rx = desired_state_tx.subscribe();
        Self {
            remote_control_url: config.remote_control_url,
            installation_id: config.installation_id,
            server_name: config.server_name,
            remote_control_target: config.remote_control_target,
            state_db,
            auth_manager,
            status_publisher: channels.status_publisher,
            shutdown_token,
            reconnect_attempt: 0,
            auth_recovery,
            auth_change_rx,
            current_enrollment: channels.current_enrollment,
            transport_enrollment: None,
            pairing_persistence_key: channels.pairing_persistence_key,
            client_tracker: Arc::new(Mutex::new(client_tracker)),
            state: Arc::new(Mutex::new(WebsocketState {
                outbound_buffer,
                subscribe_cursor: None,
                next_seq_id_by_stream: HashMap::new(),
                last_ack_activity_by_stream: HashMap::new(),
                last_completed_client_chunk_seq_id_by_stream: HashMap::new(),
                client_segment_reassembler: ClientSegmentReassembler::default(),
            })),
            server_event_rx: Arc::new(Mutex::new(server_event_rx)),
            used_rx,
            desired_state_tx,
            desired_state_rx,
            desired_state_persistence_lock: channels.desired_state_persistence_lock,
            http_clients: config.http_clients,
            policy_rx: None,
        }
    }

    /// Holds backend contact until the managed-requirements decision on `policy_rx` admits the
    /// current identity, and retries a held connection attempt when that decision changes.
    pub(crate) fn with_policy(
        mut self,
        policy_rx: watch::Receiver<RemoteControlPolicyDecision>,
    ) -> Self {
        self.policy_rx = Some(policy_rx);
        self
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "remote-control client shutdown must serialize tracker state"
    )]
    pub(crate) async fn run(
        mut self,
        app_server_client_name_rx: Option<oneshot::Receiver<String>>,
    ) {
        info!(
            remote_control_url = %self.remote_control_url,
            installation_id = %self.installation_id,
            server_name = %self.server_name,
            initial_desired_state = ?*self.desired_state_rx.borrow(),
            "app-server remote control websocket loop started"
        );
        let app_server_client_name = match self
            .wait_for_app_server_client_name(app_server_client_name_rx)
            .await
        {
            Ok(app_server_client_name) => app_server_client_name,
            Err(_) => {
                warn!(
                    remote_control_url = %self.remote_control_url,
                    installation_id = %self.installation_id,
                    server_name = %self.server_name,
                    shutdown_requested = self.shutdown_token.is_cancelled(),
                    "app-server remote control websocket loop stopped before client name was ready"
                );
                self.client_tracker.lock().await.shutdown().await;
                return;
            }
        };
        self.pairing_persistence_key
            .send_replace(app_server_client_name.clone());
        if matches!(
            *self.desired_state_rx.borrow(),
            RemoteControlDesiredState::Unknown
        ) && !self
            .resolve_unknown_desired_state(app_server_client_name.as_deref())
            .await
        {
            self.client_tracker.lock().await.shutdown().await;
            return;
        }

        loop {
            if !self.wait_until_enabled().await {
                info!(
                    remote_control_url = %self.remote_control_url,
                    installation_id = %self.installation_id,
                    server_name = %self.server_name,
                    shutdown_requested = self.shutdown_token.is_cancelled(),
                    current_status = ?self.status_publisher.status().status,
                    "app-server remote control websocket loop exiting while waiting for enablement"
                );
                break;
            }

            let status = self.status_publisher.status();
            info!(
                remote_control_url = %self.remote_control_url,
                installation_id = %self.installation_id,
                server_name = %self.server_name,
                reconnect_attempt = self.reconnect_attempt.saturating_add(1),
                current_status = ?status.status,
                environment_id = ?status.environment_id,
                "starting app-server remote control websocket connection cycle"
            );
            let shutdown_token = self.shutdown_token.child_token();
            let websocket_connection = match self
                .connect(&shutdown_token, app_server_client_name.as_deref())
                .await
            {
                ConnectOutcome::Connected(websocket_connection) => *websocket_connection,
                ConnectOutcome::Disabled => {
                    self.retire_transport().await;
                    self.status_publisher
                        .publish_status(RemoteControlConnectionStatus::Disabled);
                    continue;
                }
                ConnectOutcome::Shutdown => break,
            };

            let connection_end_reason = self
                .run_connection(websocket_connection, shutdown_token)
                .await;
            let status = self.status_publisher.status();
            info!(
                remote_control_url = %self.remote_control_url,
                installation_id = %self.installation_id,
                server_name = %self.server_name,
                connection_end_reason = ?connection_end_reason,
                current_status = ?status.status,
                environment_id = ?status.environment_id,
                desired_state = ?*self.desired_state_rx.borrow(),
                "app-server remote control websocket connection cycle ended"
            );
            if !self.wait_to_reconnect_after(connection_end_reason).await {
                break;
            }
        }

        self.client_tracker.lock().await.shutdown().await;
        info!(
            remote_control_url = %self.remote_control_url,
            installation_id = %self.installation_id,
            server_name = %self.server_name,
            shutdown_requested = self.shutdown_token.is_cancelled(),
            "app-server remote control websocket loop exited"
        );
    }

    async fn wait_to_reconnect_after(
        &mut self,
        connection_end_reason: ConnectionEndReason,
    ) -> bool {
        let Some(reconnect_delay) = reconnect_delay_after_connection_end(
            connection_end_reason,
            &mut self.reconnect_attempt,
        ) else {
            return true;
        };
        warn!(
            remote_control_url = %self.remote_control_url,
            installation_id = %self.installation_id,
            server_name = %self.server_name,
            reconnect_delay = ?reconnect_delay,
            "remote control websocket connection stopped; delaying reconnect"
        );
        Self::wait_for_reconnect_delay(
            &self.shutdown_token,
            &mut self.desired_state_rx,
            reconnect_delay,
        )
        .await
    }

    async fn wait_for_reconnect_delay(
        shutdown_token: &CancellationToken,
        desired_state_rx: &mut watch::Receiver<RemoteControlDesiredState>,
        reconnect_delay: std::time::Duration,
    ) -> bool {
        tokio::select! {
            _ = shutdown_token.cancelled() => false,
            changed = desired_state_rx.wait_for(|state| !state.is_enabled()) => {
                changed.is_ok()
            }
            _ = tokio::time::sleep(reconnect_delay) => true,
        }
    }

    async fn wait_for_app_server_client_name(
        &self,
        app_server_client_name_rx: Option<oneshot::Receiver<String>>,
    ) -> Result<Option<String>, ()> {
        match app_server_client_name_rx {
            Some(app_server_client_name_rx) => {
                tokio::select! {
                    _ = self.shutdown_token.cancelled() => Err(()),
                    app_server_client_name = app_server_client_name_rx => match app_server_client_name {
                        Ok(app_server_client_name) => Ok(Some(app_server_client_name)),
                        Err(_) => Err(()),
                    },
                }
            }
            None => Ok(None),
        }
    }

    pub(super) async fn resolve_unknown_desired_state(
        &mut self,
        app_server_client_name: Option<&str>,
    ) -> bool {
        let remote_control_target = match super::protocol::normalize_remote_control_url(
            &self.remote_control_url,
        ) {
            Ok(remote_control_target) => remote_control_target,
            Err(err) => {
                warn!(
                    "remote control preference cannot be resolved because the URL is invalid: {err}"
                );
                self.transition_unknown_to(RemoteControlDesiredState::Disabled);
                return true;
            }
        };
        self.remote_control_target = Some(remote_control_target.clone());
        let Some(state_db) = self.state_db.clone() else {
            self.transition_unknown_to(RemoteControlDesiredState::Disabled);
            return true;
        };

        let mut retry_attempt = 0;
        loop {
            if !matches!(
                *self.desired_state_rx.borrow(),
                RemoteControlDesiredState::Unknown
            ) {
                return true;
            }
            let auth = match load_remote_control_auth(&self.auth_manager).await {
                Ok(auth) => auth,
                Err(err) => {
                    info!(
                        error = %err,
                        "waiting to resolve remote control preference until authentication is available"
                    );
                    if !self
                        .wait_for_preference_resolution_retry(&mut retry_attempt)
                        .await
                    {
                        return false;
                    }
                    continue;
                }
            };
            let mut current_enrollment = self.current_enrollment.lock().await;
            let enrollment = match state_db
                .get_remote_control_enrollment(
                    &remote_control_target.websocket_url,
                    &auth.account_id,
                    app_server_client_name,
                )
                .await
            {
                Ok(enrollment) => enrollment,
                Err(err) => {
                    warn!(
                        error = %err,
                        "failed to resolve persisted remote control preference; retrying"
                    );
                    drop(current_enrollment);
                    if !self
                        .wait_for_preference_resolution_retry(&mut retry_attempt)
                        .await
                    {
                        return false;
                    }
                    continue;
                }
            };
            let desired_state = desired_state_from_persisted_enrollment(enrollment.as_ref());
            if desired_state.is_enabled()
                && matches!(
                    *self.desired_state_rx.borrow(),
                    RemoteControlDesiredState::Unknown
                )
            {
                *current_enrollment = enrollment.map(|record| {
                    RemoteControlEnrollment::from_persisted(&remote_control_target, record)
                });
            }
            drop(current_enrollment);
            self.transition_unknown_to(desired_state);
            return true;
        }
    }

    fn transition_unknown_to(&self, desired_state: RemoteControlDesiredState) {
        self.desired_state_tx.send_if_modified(|state| {
            if !matches!(*state, RemoteControlDesiredState::Unknown) {
                return false;
            }
            *state = desired_state;
            true
        });
    }

    /// Auth published by this process retries at once. The capped backoff still
    /// notices credentials written elsewhere, as the connect loop does, without
    /// reloading auth every second for a session that never signs in.
    async fn wait_for_preference_resolution_retry(&mut self, retry_attempt: &mut u64) -> bool {
        let retry_delay = next_reconnect_delay(retry_attempt);
        tokio::select! {
            _ = self.shutdown_token.cancelled() => false,
            changed = self.desired_state_rx.changed() => changed.is_ok(),
            changed = self.auth_change_rx.changed() => {
                *retry_attempt = 0;
                changed.is_ok()
            }
            _ = tokio::time::sleep(retry_delay) => true,
        }
    }

    async fn wait_until_enabled(&mut self) -> bool {
        if !self.desired_state_rx.borrow().is_enabled() {
            self.retire_transport().await;
        }
        tokio::select! {
            _ = self.shutdown_token.cancelled() => false,
            desired_state = self.desired_state_rx.wait_for(|state| state.is_enabled()) => desired_state.is_ok(),
        }
    }

    async fn connect(
        &mut self,
        shutdown_token: &CancellationToken,
        app_server_client_name: Option<&str>,
    ) -> ConnectOutcome {
        self.status_publisher
            .publish_status(RemoteControlConnectionStatus::Connecting);
        let remote_control_target = match self.remote_control_target.as_ref() {
            Some(remote_control_target) => remote_control_target.clone(),
            None => match super::protocol::normalize_remote_control_url(&self.remote_control_url) {
                Ok(remote_control_target) => {
                    self.remote_control_target = Some(remote_control_target.clone());
                    remote_control_target
                }
                Err(err) => {
                    self.status_publisher
                        .publish_status(RemoteControlConnectionStatus::Errored);
                    warn!("remote control is enabled but the URL is invalid: {err}");
                    tokio::select! {
                        _ = shutdown_token.cancelled() => return ConnectOutcome::Shutdown,
                        changed = self.desired_state_rx.wait_for(|state| !state.is_enabled()) => {
                            if changed.is_err() {
                                return ConnectOutcome::Shutdown;
                            }
                            return ConnectOutcome::Disabled;
                        }
                    }
                }
            },
        };

        let mut repaired_retry_available = true;
        loop {
            // Mark the decision seen before this attempt checks it, so a decision recorded later
            // wakes an attempt held for managed requirements.
            if let Some(policy_rx) = self.policy_rx.as_mut() {
                policy_rx.mark_unchanged();
            }
            let subscribe_cursor = self.state.lock().await.subscribe_cursor.clone();
            let enrollment = self.current_enrollment.snapshot();
            info!(
                websocket_url = %remote_control_target.websocket_url,
                installation_id = %self.installation_id,
                server_name = %self.server_name,
                reconnect_attempt = self.reconnect_attempt.saturating_add(1),
                has_enrollment = enrollment.is_some(),
                server_id = ?enrollment.as_ref().map(|enrollment| enrollment.server_id.as_str()),
                environment_id = ?enrollment.as_ref().map(|enrollment| enrollment.environment_id.as_str()),
                subscribe_cursor_present = subscribe_cursor.is_some(),
                app_server_client_name = ?app_server_client_name,
                "connecting to app-server remote control websocket"
            );
            let connect_options = RemoteControlConnectOptions {
                installation_id: &self.installation_id,
                server_name: &self.server_name,
                subscribe_cursor: self
                    .transport_enrollment
                    .as_ref()
                    .zip(subscribe_cursor.as_deref()),
                app_server_client_name,
                desired_state_tx: &self.desired_state_tx,
                desired_state_persistence_lock: &self.desired_state_persistence_lock,
                http_clients: &self.http_clients,
                request_state: &self.current_enrollment,
                policy: self.policy_rx.as_ref(),
            };
            let auth_context = RemoteControlAuthContext {
                auth_manager: &self.auth_manager,
                auth_recovery: &mut self.auth_recovery,
                auth_change_rx: &mut self.auth_change_rx,
            };
            let mut disabled_rx = self.desired_state_rx.clone();
            let connect_result = tokio::select! {
                _ = shutdown_token.cancelled() => return ConnectOutcome::Shutdown,
                changed = disabled_rx.wait_for(|state| !state.is_enabled()) => {
                    if changed.is_err() {
                        return ConnectOutcome::Shutdown;
                    }
                    return ConnectOutcome::Disabled;
                }
                connect_result = async {
                    connect_remote_control_websocket(
                        &remote_control_target,
                        self.state_db.as_deref(),
                        auth_context,
                        &self.current_enrollment,
                        connect_options,
                        &self.status_publisher,
                    )
                    .await
                } => connect_result,
            };

            match connect_result {
                Ok((websocket_connection, response, enrollment)) => {
                    if self.transport_enrollment.as_ref().is_some_and(|previous| {
                        !same_remote_control_enrollment(previous, &enrollment)
                    }) {
                        self.retire_transport().await;
                    }
                    self.transport_enrollment = Some(enrollment);
                    if !self.desired_state_rx.borrow().is_enabled() {
                        return ConnectOutcome::Disabled;
                    }
                    self.reconnect_attempt = 0;
                    self.auth_recovery = self.auth_manager.unauthorized_recovery();
                    self.status_publisher
                        .publish_status(RemoteControlConnectionStatus::Connected);
                    let enrollment = self.current_enrollment.snapshot();
                    info!(
                        websocket_url = %remote_control_target.websocket_url,
                        installation_id = %self.installation_id,
                        server_name = %self.server_name,
                        server_id = ?enrollment.as_ref().map(|enrollment| enrollment.server_id.as_str()),
                        environment_id = ?enrollment.as_ref().map(|enrollment| enrollment.environment_id.as_str()),
                        subscribe_cursor_present = subscribe_cursor.is_some(),
                        response_headers = %format_headers(response.headers()),
                        "connected to app-server remote control websocket"
                    );
                    return ConnectOutcome::Connected(Box::new(websocket_connection));
                }
                Err(err) => {
                    if !self.desired_state_rx.borrow().is_enabled() {
                        return ConnectOutcome::Disabled;
                    }
                    if take_repaired_retry(&err, &mut repaired_retry_available) {
                        continue;
                    }
                    let reconnect_delay = if let Some(deadline) = self.current_enrollment.retry_after() {
                        (deadline - time::OffsetDateTime::now_utc()).try_into().unwrap_or_default()
                    } else if err.kind() == ErrorKind::WouldBlock {
                        self.current_enrollment
                            .snapshot()
                            .and_then(|enrollment| enrollment.next_refresh_at)
                            .and_then(|deadline| {
                                (deadline - time::OffsetDateTime::now_utc()).try_into().ok()
                            })
                            .unwrap_or(REMOTE_CONTROL_ACCOUNT_ID_RETRY_INTERVAL)
                    } else {
                        self.status_publisher
                            .publish_status(RemoteControlConnectionStatus::Errored);
                        let reconnect_attempt = self.reconnect_attempt.saturating_add(1);
                        let reconnect_delay = next_reconnect_delay(&mut self.reconnect_attempt);
                        let enrollment = self.current_enrollment.snapshot();
                        warn!(
                            websocket_url = %remote_control_target.websocket_url,
                            installation_id = %self.installation_id,
                            server_name = %self.server_name,
                            error = %err,
                            error_kind = ?err.kind(),
                            reconnect_attempt,
                            reconnect_delay = ?reconnect_delay,
                            has_enrollment = enrollment.is_some(),
                            server_id = ?enrollment.as_ref().map(|enrollment| enrollment.server_id.as_str()),
                            environment_id = ?enrollment.as_ref().map(|enrollment| enrollment.environment_id.as_str()),
                            subscribe_cursor_present = subscribe_cursor.is_some(),
                            "failed to connect to app-server remote control websocket"
                        );
                        reconnect_delay
                    };
                    tokio::select! {
                        _ = shutdown_token.cancelled() => return ConnectOutcome::Shutdown,
                        changed = self.desired_state_rx.wait_for(|state| !state.is_enabled()) => {
                            if changed.is_err() {
                                return ConnectOutcome::Shutdown;
                            }
                            return ConnectOutcome::Disabled;
                        }
                        changed = self.auth_change_rx.changed() => {
                            if changed.is_err() {
                                return ConnectOutcome::Shutdown;
                            }
                            self.auth_recovery = self.auth_manager.unauthorized_recovery();
                            self.reconnect_attempt = 0;
                            info!("retrying app-server remote control websocket after auth changed");
                        }
                        () = wait_for_policy_change(&mut self.policy_rx) => {
                            info!("retrying app-server remote control websocket after managed requirements changed");
                        }
                        _ = tokio::time::sleep(reconnect_delay) => {}
                    }
                    repaired_retry_available = true;
                }
            }
        }
    }

    async fn run_connection(
        &mut self,
        websocket_connection: WebSocketStream<MaybeTlsStream<TcpStream>>,
        shutdown_token: CancellationToken,
    ) -> ConnectionEndReason {
        let (websocket_writer, websocket_reader) = websocket_connection.split();
        let mut join_set = tokio::task::JoinSet::new();

        join_set.spawn(Self::run_server_writer(
            self.state.clone(),
            self.server_event_rx.clone(),
            self.used_rx.clone(),
            websocket_writer,
            REMOTE_CONTROL_WEBSOCKET_PING_INTERVAL,
            shutdown_token.clone(),
        ));
        join_set.spawn(Self::run_websocket_reader(
            self.client_tracker.clone(),
            self.state.clone(),
            websocket_reader,
            REMOTE_CONTROL_WEBSOCKET_PONG_TIMEOUT,
            shutdown_token.clone(),
        ));

        let mut desired_state_rx = self.desired_state_rx.clone();
        let account_id = self
            .transport_enrollment
            .as_ref()
            .map(|enrollment| enrollment.account_id.clone());
        let auth_changed = async {
            loop {
                // Check before waiting too: an auth change may have arrived during the handshake.
                let auth = load_remote_control_auth(&self.auth_manager).await;
                if auth.as_ref().ok().map(|auth| &auth.account_id) != account_id.as_ref() {
                    return;
                }
                if self.auth_change_rx.changed().await.is_err() {
                    return;
                }
            }
        };
        let connection_end_reason = tokio::select! {
            _ = auth_changed => ConnectionEndReason::AuthChanged,
            _ = shutdown_token.cancelled() => ConnectionEndReason::Shutdown,
            changed = desired_state_rx.wait_for(|state| !state.is_enabled()) => {
                if changed.is_ok() {
                    self.status_publisher
                        .publish_status(RemoteControlConnectionStatus::Disabled);
                    ConnectionEndReason::Disabled
                } else {
                    ConnectionEndReason::EnabledWatchClosed
                }
            }
            _ = join_set.join_next() => ConnectionEndReason::ConnectionWorkerStopped,
        };
        shutdown_token.cancel();

        Self::join_connection_workers(&mut join_set, REMOTE_CONTROL_CONNECTION_SHUTDOWN_TIMEOUT)
            .await;
        if matches!(
            connection_end_reason,
            ConnectionEndReason::AuthChanged | ConnectionEndReason::Disabled
        ) || self.state.lock().await.next_seq_id_by_stream.len() >= MAX_TRANSPORT_STREAM_IDENTITIES {
            self.retire_transport().await;
        }
        connection_end_reason
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serialize client cancellation against tracker access while closing all virtual clients"
    )]
    async fn retire_transport(&mut self) {
        // Workers have stopped. Cancel virtual clients before draining their queued output.
        self.client_tracker.lock().await.retire_clients();
        {
            let mut queued = self.server_event_rx.lock().await;
            while queued.try_recv().is_ok() {}
        }
        let mut state = self.state.lock().await;
        state.outbound_buffer.buffer_by_stream.clear();
        state.outbound_buffer.used_tx.send_replace(0);
        state.subscribe_cursor = None;
        state.next_seq_id_by_stream.clear();
        state.last_ack_activity_by_stream.clear();
        state.last_completed_client_chunk_seq_id_by_stream.clear();
        state.client_segment_reassembler = ClientSegmentReassembler::default();
        self.transport_enrollment = None;
    }

    async fn join_connection_workers(
        join_set: &mut tokio::task::JoinSet<()>,
        shutdown_timeout: std::time::Duration,
    ) {
        if tokio::time::timeout(shutdown_timeout, Self::drain_join_set(join_set))
            .await
            .is_ok()
        {
            return;
        }

        warn!(
            shutdown_timeout = ?shutdown_timeout,
            remaining_workers = join_set.len(),
            "timed out waiting for remote control connection workers to stop; aborting"
        );
        join_set.abort_all();
        Self::drain_join_set(join_set).await;
    }

    async fn drain_join_set(join_set: &mut tokio::task::JoinSet<()>) {
        while join_set.join_next().await.is_some() {}
    }

    async fn run_server_writer(
        state: Arc<Mutex<WebsocketState>>,
        server_event_rx: Arc<Mutex<mpsc::Receiver<super::QueuedServerEnvelope>>>,
        used_rx: watch::Receiver<usize>,
        websocket_writer: SplitSink<
            WebSocketStream<MaybeTlsStream<TcpStream>>,
            tungstenite::Message,
        >,
        ping_interval: std::time::Duration,
        shutdown_token: CancellationToken,
    ) {
        let result = Self::run_server_writer_inner(
            state,
            server_event_rx,
            used_rx,
            websocket_writer,
            ping_interval,
            shutdown_token,
        )
        .await;
        if let Err(err) = result {
            warn!("remote control websocket writer disconnected, err: {err}");
        } else {
            warn!("remote control websocket writer was stopped");
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "remote-control server event receiver is shared across reconnects"
    )]
    async fn run_server_writer_inner(
        state: Arc<Mutex<WebsocketState>>,
        server_event_rx: Arc<Mutex<mpsc::Receiver<super::QueuedServerEnvelope>>>,
        mut used_rx: watch::Receiver<usize>,
        mut websocket_writer: impl futures::Sink<tungstenite::Message, Error = tungstenite::Error>
        + Unpin,
        ping_interval: std::time::Duration,
        shutdown_token: CancellationToken,
    ) -> io::Result<()> {
        let mut pending = ServerWriterQueue::default();
        {
            let state = state.lock().await;
            for ((client_id, stream_id), envelopes) in &state.outbound_buffer.buffer_by_stream {
                for envelope in envelopes {
                    pending.push(
                        (client_id.clone(), stream_id.clone()),
                        envelope.payload.clone(),
                        None,
                    );
                }
            }
        }
        // Heartbeats exist before replay, and every bounded batch returns here.
        let mut ping_interval =
            tokio::time::interval_at(tokio::time::Instant::now() + ping_interval, ping_interval);
        ping_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut server_event_rx = server_event_rx.lock().await;
        // One additional logical envelope may compete with a saturated replay
        // backlog. Its existing per-envelope frame/byte bounds still apply;
        // another cannot enter until this envelope is ACKed or capacity returns.
        let mut fairness_overdraft: Option<((ClientId, StreamId), u64)> = None;
        loop {
            if *used_rx.borrow() < super::CHANNEL_CAPACITY {
                fairness_overdraft = None;
            } else if let Some((key, seq_id)) = &fairness_overdraft {
                let retained = state.lock().await.outbound_buffer.buffer_by_stream.get(key)
                    .is_some_and(|frames| frames.iter().any(|frame| frame.seq_id == *seq_id));
                if !retained { fairness_overdraft = None; }
            }
            // Drain only already-ready work. Never wait to fill a batch, and never
            // admit more original envelopes after the replay capacity is consumed.
            for _ in 0..REMOTE_CONTROL_REPLAY_BATCH_MESSAGES {
                let saturated = *used_rx.borrow() >= super::CHANNEL_CAPACITY;
                if saturated && fairness_overdraft.is_some() {
                    break;
                }
                match server_event_rx.try_recv() {
                    Ok(envelope) => {
                        let admitted = pending.enqueue(&state, envelope).await?;
                        if saturated { fairness_overdraft = Some(admitted); }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) if pending.is_empty() => {
                        return Err(io::Error::new(ErrorKind::UnexpectedEof, "server event channel closed"));
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                }
            }

            let outbound_has_capacity = *used_rx.borrow() < super::CHANNEL_CAPACITY || fairness_overdraft.is_none();
            tokio::select! {
                biased;
                _ = shutdown_token.cancelled() => return Ok(()),
                _ = ping_interval.tick() => {
                    writer_operation(&shutdown_token, websocket_writer.send(tungstenite::Message::Ping(Vec::new().into()))).await?;
                    continue;
                }
                _ = std::future::ready(()), if !pending.is_empty() => {}
                changed = used_rx.changed(), if !outbound_has_capacity => {
                    changed.map_err(|_| io::Error::new(ErrorKind::UnexpectedEof, "outbound buffer usage channel closed"))?;
                    continue;
                }
                envelope = server_event_rx.recv(), if outbound_has_capacity => {
                    let envelope = envelope.ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "server event channel closed"))?;
                    let saturated = *used_rx.borrow() >= super::CHANNEL_CAPACITY;
                    let admitted = pending.enqueue(&state, envelope).await?;
                    if saturated { fairness_overdraft = Some(admitted); }
                    continue;
                }
            }

            let mut batch_messages = 0;
            let mut batch_bytes = 0;
            let mut completions = Vec::new();
            let mut heartbeat_due = false;
            while batch_messages < REMOTE_CONTROL_REPLAY_BATCH_MESSAGES
                && batch_bytes < REMOTE_CONTROL_REPLAY_BATCH_TARGET_BYTES
            {
                // A frame-count budget alone is not a time budget on a slow
                // sink. Observe the heartbeat between individual committed feeds.
                tokio::select! {
                    biased;
                    _ = shutdown_token.cancelled() => return Ok(()),
                    _ = ping_interval.tick() => { heartbeat_due = true; break; }
                    _ = std::future::ready(()) => {}
                }
                let Some(frame) = pending.pop() else { break };
                batch_messages += 1;
                batch_bytes += frame.payload.len();
                writer_operation(&shutdown_token, websocket_writer.feed(tungstenite::Message::Text(frame.payload))).await?;
                if shutdown_token.is_cancelled() { return Ok(()); }
                if let Some(completion) = frame.completion {
                    completions.push(completion);
                }
            }
            writer_operation(&shutdown_token, websocket_writer.flush()).await?;
            if shutdown_token.is_cancelled() { return Ok(()); }
            for completion in completions {
                let _ = completion.send(std::time::Instant::now());
            }
            if heartbeat_due {
                writer_operation(&shutdown_token, websocket_writer.send(tungstenite::Message::Ping(Vec::new().into()))).await?;
            }
        }
    }

    async fn run_websocket_reader(
        client_tracker: Arc<Mutex<ClientTracker>>,
        state: Arc<Mutex<WebsocketState>>,
        websocket_reader: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        pong_timeout: std::time::Duration,
        shutdown_token: CancellationToken,
    ) {
        let result = Self::run_websocket_reader_inner(
            client_tracker,
            state,
            websocket_reader,
            pong_timeout,
            shutdown_token,
        )
        .await;
        if let Err(err) = result {
            warn!("remote control websocket reader disconnected, err: {err}");
        } else {
            warn!("remote control websocket reader was stopped");
        }
    }

    async fn run_websocket_reader_inner(
        client_tracker: Arc<Mutex<ClientTracker>>,
        state: Arc<Mutex<WebsocketState>>,
        mut websocket_reader: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        pong_timeout: std::time::Duration,
        shutdown_token: CancellationToken,
    ) -> io::Result<()> {
        let (ingress_tx, ingress_rx) = mpsc::channel(super::CHANNEL_CAPACITY);
        let mut ingress_worker = AbortOnDropHandle::new(tokio::spawn(run_client_ingress(
            client_tracker, state.clone(), ingress_rx, shutdown_token.clone(),
        )));
        let mut pending = VecDeque::<PendingClientDelivery>::new();
        // Completed chunk operations awaiting admission are not yet committed
        // dedupe/cursor state, but a retransmission must not initialize twice.
        let mut in_flight_chunks = HashMap::new();
        let mut next_delivery_ordinal = 0usize;
        let mut pending_bytes = 0usize;
        let mut assembly_sweep = tokio::time::interval(REMOTE_CONTROL_IDLE_SWEEP_INTERVAL);
        assembly_sweep.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let pong_deadline = tokio::time::sleep(pong_timeout);
        tokio::pin!(pong_deadline);
        loop {
            let incoming_message = tokio::select! {
                _ = shutdown_token.cancelled() => return Ok(()),
                _ = &mut ingress_worker => {
                    if shutdown_token.is_cancelled() { return Ok(()); }
                    return Err(io::Error::new(ErrorKind::BrokenPipe, "remote control ingress worker stopped"));
                }
                _ = &mut pong_deadline => return Err(io::Error::new(ErrorKind::TimedOut, "remote control websocket pong timeout")),
                _ = assembly_sweep.tick() => {
                    state.lock().await.client_segment_reassembler.expire_assemblies();
                    continue;
                }
                result = async { (&mut pending.front_mut().expect("nonempty frontier").completion).await }, if !pending.is_empty() => {
                    if shutdown_token.is_cancelled() { return Ok(()); }
                    let delivered = result.map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "remote control ingress worker stopped"))?
                        .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "remote control ingress admission failed"))?;
                    let mut delivery = pending.pop_front().expect("completed frontier");
                    pending_bytes -= delivery.bytes;
                    if let Some(key) = delivery.client_message_key.as_ref() {
                        if in_flight_chunks.get(key) == Some(&delivery.ordinal) {
                            in_flight_chunks.remove(key);
                        } else {
                            // A receive-order close retired this operation's
                            // generation. Commit its cursor, not stale dedupe.
                            delivery.client_message_key = None;
                        }
                    }
                    delivery.commit(&mut *state.lock().await, delivered);
                    continue;
                }
                incoming_message = websocket_reader.next() => {
                    match incoming_message {
                        Some(incoming_message) => incoming_message,
                        None => return Err(io::Error::new(ErrorKind::UnexpectedEof, "websocket stream ended")),
                    }
                }
            };
            let (client_envelope, wire_size_bytes) = match incoming_message {
                Ok(tungstenite::Message::Text(text)) => {
                    let wire_size_bytes = text.len();
                    match serde_json::from_str::<ClientEnvelope>(&text) {
                        Ok(client_envelope) => (client_envelope, wire_size_bytes),
                        Err(err) => {
                            warn!("failed to deserialize remote-control client event: {err}");
                            continue;
                        }
                    }
                }
                Ok(tungstenite::Message::Pong(_)) => {
                    pong_deadline
                        .as_mut()
                        .reset(tokio::time::Instant::now() + pong_timeout);
                    continue;
                }
                Ok(tungstenite::Message::Ping(_)) | Ok(tungstenite::Message::Frame(_)) => continue,
                Ok(tungstenite::Message::Binary(_)) => {
                    warn!("dropping unsupported binary remote-control websocket message");
                    continue;
                }
                Ok(tungstenite::Message::Close(_)) => {
                    return Err(io::Error::new(
                        ErrorKind::ConnectionAborted,
                        "websocket disconnected",
                    ));
                }
                Err(err) => {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!("failed to read from websocket: {err}"),
                    ));
                }
            };


            let bytes = match &client_envelope.event {
                ClientEvent::ClientMessageChunk { message_size_bytes, .. } => *message_size_bytes,
                _ => wire_size_bytes,
            };
            let client_message_key = WebsocketState::client_message_key(&client_envelope);
            if client_message_key.as_ref().is_some_and(|key| in_flight_chunks.contains_key(key)) {
                continue;
            }
            if matches!(&client_envelope.event, ClientEvent::ClientClosed) {
                // This boundary is in wire order, not admission-completion order:
                // later chunks can already be assembling while close delivery waits.
                let client_id = &client_envelope.client_id;
                let stream_id = client_envelope.stream_id.as_ref();
                let mut state = state.lock().await;
                if let Some(stream_id) = stream_id {
                    state.client_segment_reassembler.invalidate_stream(client_id, stream_id);
                    state.invalidate_client_message_stream(client_id, stream_id);
                } else {
                    state.client_segment_reassembler.invalidate_client(client_id);
                    state.invalidate_client_message_client(client_id);
                }
                in_flight_chunks.retain(|((client, stream), _), _| {
                    client != client_id || stream_id.is_some_and(|closed| stream.as_ref() != Some(closed))
                });
            }
            let observation = state.lock().await.observe_client_message(client_envelope, wire_size_bytes);
            let client_envelope = match observation {
                ClientSegmentObservation::Forward(envelope) => *envelope,
                ClientSegmentObservation::Pending | ClientSegmentObservation::Dropped => continue,
            };
            // Bound both queued operations and their declared/reassembled byte cost.
            // Overflow is explicit failure; no unadmitted message commits its cursor.
            if pending.len() >= super::CHANNEL_CAPACITY
                || bytes > super::segment::REMOTE_CONTROL_REASSEMBLED_MAX_BYTES.saturating_sub(pending_bytes)
            {
                return Err(io::Error::new(ErrorKind::WouldBlock, "remote control ingress forwarding queue exhausted"));
            }
            let mut delivery = ClientDeliveryMetadata::from(&client_envelope);
            let (completion_tx, completion) = oneshot::channel();
            if let Some((client_id, stream_id, seq_id, segment_id)) = delivery.ack.take() {
                // ACK resource effects are independent of application admission,
                // but its subscribe cursor remains behind the completed prefix.
                state.lock().await.record_ack(&client_id, &stream_id, seq_id, segment_id);
                let _ = completion_tx.send(Ok(false));
            } else {
                ingress_tx.try_send((client_envelope, completion_tx))
                    .map_err(|_| io::Error::new(ErrorKind::WouldBlock, "remote control ingress forwarding unavailable"))?;
            }
            pending_bytes += bytes;
            let ordinal = next_delivery_ordinal;
            next_delivery_ordinal += 1;
            if let Some(key) = client_message_key.as_ref() {
                in_flight_chunks.insert(key.clone(), ordinal);
            }
            pending.push_back(PendingClientDelivery { delivery, client_message_key, ordinal, completion, bytes });
        }
    }
}

fn set_remote_control_header(
    headers: &mut tungstenite::http::HeaderMap,
    name: &'static str,
    value: &str,
) -> io::Result<()> {
    let header_value = HeaderValue::from_str(value).map_err(|err| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid remote control header `{name}`: {err}"),
        )
    })?;
    headers.insert(name, header_value);
    Ok(())
}

fn build_remote_control_websocket_request(
    websocket_url: &str,
    enrollment: &RemoteControlEnrollment,
    installation_id: &str,
    subscribe_cursor: Option<&str>,
) -> io::Result<tungstenite::http::Request<()>> {
    let mut request = websocket_url.into_client_request().map_err(|err| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid remote control websocket URL `{websocket_url}`: {err}"),
        )
    })?;
    let headers = request.headers_mut();
    headers.extend(default_headers());
    set_remote_control_header(headers, "user-agent", &remote_control_user_agent())?;
    set_remote_control_header(headers, "x-codex-server-id", &enrollment.server_id)?;
    set_remote_control_header(
        headers,
        "x-codex-name",
        &base64::engine::general_purpose::STANDARD.encode(&enrollment.server_name),
    )?;
    set_remote_control_header(
        headers,
        "x-codex-protocol-version",
        REMOTE_CONTROL_PROTOCOL_VERSION,
    )?;
    set_remote_control_header(
        headers,
        "authorization",
        &format!(
            "Bearer {}",
            enrollment
                .remote_control_token
                .as_deref()
                .ok_or_else(|| io::Error::other("missing remote control server token"))?
        ),
    )?;
    set_remote_control_header(
        headers,
        REMOTE_CONTROL_INSTALLATION_ID_HEADER,
        installation_id,
    )?;
    if let Some(subscribe_cursor) = subscribe_cursor {
        set_remote_control_header(
            headers,
            REMOTE_CONTROL_SUBSCRIBE_CURSOR_HEADER,
            subscribe_cursor,
        )?;
    }
    Ok(request)
}

fn next_reconnect_delay(reconnect_attempt: &mut u64) -> std::time::Duration {
    let reconnect_delay = capped_backoff(
        std::time::Duration::from_millis(200),
        (*reconnect_attempt).max(1),
        std::time::Duration::from_secs(30),
    )
    .min(REMOTE_CONTROL_RECONNECT_BACKOFF_CAP);
    if reconnect_delay < REMOTE_CONTROL_RECONNECT_BACKOFF_CAP {
        *reconnect_attempt = (*reconnect_attempt).saturating_add(1);
    }
    reconnect_delay
}

fn reconnect_delay_after_connection_end(
    connection_end_reason: ConnectionEndReason,
    reconnect_attempt: &mut u64,
) -> Option<std::time::Duration> {
    matches!(
        connection_end_reason,
        ConnectionEndReason::ConnectionWorkerStopped
    )
    .then(|| next_reconnect_delay(reconnect_attempt))
}

/// Resolves when the managed-requirements decision changes. A websocket driven without a policy
/// never wakes here.
async fn wait_for_policy_change(
    policy_rx: &mut Option<watch::Receiver<RemoteControlPolicyDecision>>,
) {
    let Some(policy_rx) = policy_rx else {
        return std::future::pending().await;
    };
    if policy_rx.changed().await.is_err() {
        // The handle and its monitor own the sender for as long as this websocket runs.
        std::future::pending::<()>().await;
    }
}

pub(super) async fn connect_remote_control_websocket(
    remote_control_target: &RemoteControlTarget,
    state_db: Option<&StateRuntime>,
    mut auth_context: RemoteControlAuthContext<'_>,
    current_enrollment: &CurrentRemoteControlEnrollment,
    connect_options: RemoteControlConnectOptions<'_>,
    status_publisher: &RemoteControlStatusPublisher,
) -> io::Result<(
    WebSocketStream<MaybeTlsStream<TcpStream>>,
    tungstenite::http::Response<()>,
    RemoteControlEnrollment,
)> {
    // CA reads and native-root initialization may block. Keep them off the
    // connection task, before enrollment can change state or contact the server.
    let prepare_connector = AbortOnDropHandle::new(tokio::task::spawn_blocking(|| {
        ensure_rustls_crypto_provider();
        maybe_build_rustls_client_config_with_custom_ca()
    }));
    let connector =
        tokio::time::timeout(REMOTE_CONTROL_WEBSOCKET_CONNECT_TIMEOUT, prepare_connector)
            .await
            .map_err(|_| {
                io::Error::new(
                    ErrorKind::TimedOut,
                    "timed out preparing remote control websocket TLS configuration",
                )
            })?
            .map_err(io::Error::other)?
            .map_err(io::Error::from)?
            .map(Connector::Rustls);

    let (auth, enrollment) = {
        let mut current_enrollment = current_enrollment.lock().await;
        let auth = prepare_remote_control_enrollment(
            remote_control_target,
            state_db,
            &mut auth_context,
            &mut current_enrollment,
            connect_options,
            status_publisher,
        )
        .await?;
        let enrollment = current_enrollment.as_ref().cloned().ok_or_else(|| {
            io::Error::other("missing remote control enrollment after enrollment step")
        })?;
        (auth, enrollment)
    };
    let request = build_remote_control_websocket_request(
        &remote_control_target.websocket_url,
        &enrollment,
        connect_options.installation_id,
        connect_options
            .subscribe_cursor
            .filter(|(previous, _)| same_remote_control_enrollment(previous, &enrollment))
            .map(|(_, cursor)| cursor),
    )?;

    current_enrollment.admit_request()?;
    let websocket_connect_result = tokio::time::timeout(
        REMOTE_CONTROL_WEBSOCKET_CONNECT_TIMEOUT,
        connect_async_tls_with_config(
            request, /*config*/ None, /*disable_nagle*/ false, connector,
        ),
    )
    .await
    .map_err(|_| {
        io::Error::new(
            ErrorKind::TimedOut,
            format!(
                "timed out connecting to remote control websocket at `{}` after {:?}",
                remote_control_target.websocket_url, REMOTE_CONTROL_WEBSOCKET_CONNECT_TIMEOUT
            ),
        )
    })?;

    match websocket_connect_result {
        Ok((websocket_stream, response)) => {
            Ok((websocket_stream, response.map(|_| ()), enrollment))
        }
        Err(err) => {
            if let tungstenite::Error::Http(response) = &err {
                current_enrollment.record_retry_after(response.status(), response.headers());
            }
            match &err {
                tungstenite::Error::Http(response)
                    if websocket_response_reports_missing_remote_app_server(response) =>
                {
                    info!(
                        "remote control websocket returned HTTP 404; replacing stale enrollment: websocket_url={}, account_id={}, server_id={}, environment_id={}",
                        remote_control_target.websocket_url,
                        auth.account_id,
                        enrollment.server_id,
                        enrollment.environment_id
                    );
                    replace_remote_control_enrollment_if_matches(
                        state_db,
                        remote_control_target,
                        RemoteControlEnrollmentAuthContext {
                            auth: &auth,
                            recovery: &mut auth_context,
                        },
                        current_enrollment,
                        &enrollment,
                        connect_options,
                        status_publisher,
                    )
                    .await?;
                    return Err(state_repaired_error("remote control enrollment replaced; retrying with repaired state".to_string()));
                }
                tungstenite::Error::Http(response) if response.status().as_u16() == 404 => {
                    let response_body = response
                        .body()
                        .as_deref()
                        .map(preview_remote_control_response_body)
                        .unwrap_or_else(|| "<missing>".to_string());
                    warn!(
                        websocket_url = %remote_control_target.websocket_url,
                        account_id = %auth.account_id,
                        server_id = %enrollment.server_id,
                        environment_id = %enrollment.environment_id,
                        response_status = %response.status(),
                        response_headers = %format_headers(response.headers()),
                        response_body = %response_body,
                        "remote control websocket returned unrecognized HTTP 404; preserving enrollment before retry"
                    );
                }
                tungstenite::Error::Http(response)
                    if matches!(response.status().as_u16(), 401 | 403) =>
                {
                    clear_remote_control_server_token_if_matches(current_enrollment, &enrollment)
                        .await?;
                    return Err(io::Error::other(format!(
                        "remote control websocket auth failed with HTTP {}; refreshing server token before reconnect",
                        response.status()
                    )));
                }
                _ => {}
            }
            Err(io::Error::other(
                format_remote_control_websocket_connect_error(
                    &remote_control_target.websocket_url,
                    &err,
                ),
            ))
        }
    }
}

async fn prepare_remote_control_enrollment(
    remote_control_target: &RemoteControlTarget,
    state_db: Option<&StateRuntime>,
    auth_context: &mut RemoteControlAuthContext<'_>,
    enrollment: &mut Option<RemoteControlEnrollment>,
    connect_options: RemoteControlConnectOptions<'_>,
    status_publisher: &RemoteControlStatusPublisher,
) -> io::Result<RemoteControlConnectionAuth> {
    let Some(state_db) = state_db else {
        *enrollment = None;
        return Err(io::Error::new(
            ErrorKind::NotFound,
            "remote control requires sqlite state db",
        ));
    };

    let auth = match load_remote_control_auth(auth_context.auth_manager).await {
        Ok(auth) => auth,
        Err(err) => {
            if err.kind() == ErrorKind::PermissionDenied {
                // Preserve account ownership of the durable preference while logged out.
                if let Some(enrollment) = enrollment.as_mut() {
                    enrollment.clear_server_token();
                }
                status_publisher.publish_environment_id(/*environment_id*/ None);
            }
            return Err(err);
        }
    };
    if connect_options
        .policy
        .is_some_and(|policy| !policy_admits_current_identity(policy, auth_context.auth_manager))
    {
        // The managed-requirements decision predates this account id (or denies remote
        // control). Contact no backend for the account until a decision for the current policy
        // epoch admits it; the connect loop retries when the decision changes.
        return Err(io::Error::new(
            ErrorKind::WouldBlock,
            "remote control is waiting for managed requirements for the current account",
        ));
    }
    let enrollment_account_id = enrollment.as_ref().map(|enrollment| &enrollment.account_id);
    if enrollment_account_id.is_some_and(|account_id| account_id != &auth.account_id) {
        resolve_desired_state_after_account_change(
            state_db,
            remote_control_target,
            auth_context.auth_manager,
            &auth.account_id,
            connect_options,
        )
        .await?;
        info!(
            "clearing in-memory remote control enrollment because account id changed: websocket_url={}, previous_account_id={:?}, current_account_id={:?}",
            remote_control_target.websocket_url,
            enrollment
                .as_ref()
                .map(|enrollment| enrollment.account_id.as_str()),
            auth.account_id
        );
        *enrollment = None;
        status_publisher.publish_environment_id(/*environment_id*/ None);
        if !connect_options.desired_state_tx.borrow().is_enabled() {
            return Err(io::Error::new(
                ErrorKind::Interrupted,
                "remote control disabled after account changed",
            ));
        }
    }
    if let Some(enrollment) = enrollment.as_mut() {
        enrollment.remote_control_target = remote_control_target.clone();
        enrollment.server_name = connect_options.server_name.to_string();
    }

    if let Some(enrollment) = enrollment.as_ref() {
        status_publisher.publish_environment_id(Some(enrollment.environment_id.clone()));
    }

    if enrollment.is_none() {
        let loaded_enrollment = load_persisted_remote_control_enrollment(
            Some(state_db),
            remote_control_target,
            &auth.account_id,
            connect_options.app_server_client_name,
        )
        .await?;
        if let Some(loaded_enrollment) = loaded_enrollment.as_ref() {
            status_publisher.publish_environment_id(Some(loaded_enrollment.environment_id.clone()));
        }
        *enrollment = loaded_enrollment.map(|mut enrollment| {
            enrollment.server_name = connect_options.server_name.to_string();
            enrollment
        });
    }

    enroll_and_persist_remote_control_server(
        remote_control_target,
        state_db,
        RemoteControlEnrollmentAuthContext {
            auth: &auth,
            recovery: auth_context,
        },
        enrollment,
        connect_options,
        status_publisher,
        RemoteControlEnrollmentSelection::ReuseOrCreate,
    )
    .await?;

    if enrollment
        .as_ref()
        .ok_or_else(|| io::Error::other("missing remote control enrollment after enrollment step"))?
        .should_refresh_server_token()
    {
        let enrollment_ref = enrollment.as_ref().ok_or_else(|| {
            io::Error::other("missing remote control enrollment after enrollment step")
        })?;
        let server_id = enrollment_ref.server_id.clone();
        let environment_id = enrollment_ref.environment_id.clone();

        info!(
            "refreshing remote control server token: websocket_url={}, refresh_url={}, account_id={}, server_id={}, environment_id={}",
            remote_control_target.websocket_url,
            remote_control_target.refresh_url,
            auth.account_id,
            server_id,
            environment_id
        );
        let enrollment_ref = enrollment.as_mut().ok_or_else(|| {
            io::Error::other("missing remote control enrollment before server refresh")
        })?;
        let client = connect_options
            .http_clients
            .client_for_url(&remote_control_target.refresh_url)
            .await
            .map_err(io::Error::other)?;
        match refresh_remote_control_server_with_client(
            &client,
            connect_options.request_state,
            &auth,
            connect_options.installation_id,
            enrollment_ref,
        )
        .await
        {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {
                info!(
                    "remote control server refresh returned HTTP 404; replacing stale enrollment: websocket_url={}, account_id={}, server_id={}, environment_id={}",
                    remote_control_target.websocket_url, auth.account_id, server_id, environment_id
                );
                enroll_and_persist_remote_control_server(
                    remote_control_target,
                    state_db,
                    RemoteControlEnrollmentAuthContext {
                        auth: &auth,
                        recovery: auth_context,
                    },
                    enrollment,
                    connect_options,
                    status_publisher,
                    RemoteControlEnrollmentSelection::ReplaceExisting,
                )
                .await?;
            }
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                if recover_remote_control_auth(
                    auth_context.auth_recovery,
                    auth_context.auth_change_rx,
                    &auth,
                )
                .await
                {
                    return Err(state_repaired_error(format!(
                        "{err}; retrying after auth recovery"
                    )));
                }
                enrollment_ref.clear_server_token();
                return Err(err);
            }
            Err(err) => return Err(err),
        }
    }

    Ok(auth)
}

async fn resolve_desired_state_after_account_change(
    state_db: &StateRuntime,
    remote_control_target: &RemoteControlTarget,
    auth_manager: &Arc<AuthManager>,
    account_id: &str,
    connect_options: RemoteControlConnectOptions<'_>,
) -> io::Result<()> {
    let durable_enabled = RemoteControlDesiredState::Enabled {
        persistence_preference: Some(true),
    };
    if *connect_options.desired_state_tx.borrow() != durable_enabled {
        return Ok(());
    }

    let _persistence =
        acquire_persistence_lock(connect_options.desired_state_persistence_lock).await;
    if *connect_options.desired_state_tx.borrow() != durable_enabled {
        return Ok(());
    }
    let enrollment = state_db
        .get_remote_control_enrollment(
            &remote_control_target.websocket_url,
            account_id,
            connect_options.app_server_client_name,
        )
        .await
        .map_err(io::Error::other)?;
    let current_auth = load_remote_control_auth(auth_manager).await?;
    if current_auth.account_id != account_id {
        return Err(io::Error::new(
            ErrorKind::WouldBlock,
            "remote control account changed while resolving persisted preference",
        ));
    }
    let resolved_state = desired_state_from_persisted_enrollment(enrollment.as_ref());
    connect_options.desired_state_tx.send_if_modified(|state| {
        if *state != durable_enabled || *state == resolved_state {
            return false;
        }
        *state = resolved_state;
        true
    });
    Ok(())
}

fn websocket_response_reports_missing_remote_app_server(
    response: &tungstenite::http::Response<Option<Vec<u8>>>,
) -> bool {
    response.status().as_u16() == 404
        && response.body().as_deref().is_some_and(|body| {
            serde_json::from_slice::<serde_json::Value>(body).is_ok_and(|body| {
                body.get("detail").and_then(serde_json::Value::as_str)
                    == Some(REMOTE_APP_SERVER_NOT_FOUND_DETAIL)
            })
        })
}

async fn replace_remote_control_enrollment_if_matches(
    state_db: Option<&StateRuntime>,
    remote_control_target: &RemoteControlTarget,
    auth_context: RemoteControlEnrollmentAuthContext<'_, '_>,
    current_enrollment: &CurrentRemoteControlEnrollment,
    enrollment: &RemoteControlEnrollment,
    connect_options: RemoteControlConnectOptions<'_>,
    status_publisher: &RemoteControlStatusPublisher,
) -> io::Result<()> {
    let Some(state_db) = state_db else {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            "remote control requires sqlite state db",
        ));
    };
    let mut current_enrollment = current_enrollment.lock().await;
    if !current_enrollment
        .as_ref()
        .is_some_and(|current| same_remote_control_enrollment(current, enrollment))
    {
        return Ok(());
    }
    enroll_and_persist_remote_control_server(
        remote_control_target,
        state_db,
        auth_context,
        &mut current_enrollment,
        connect_options,
        status_publisher,
        RemoteControlEnrollmentSelection::ReplaceExisting,
    )
    .await
}

async fn clear_remote_control_server_token_if_matches(
    current_enrollment: &CurrentRemoteControlEnrollment,
    enrollment: &RemoteControlEnrollment,
) -> io::Result<()> {
    let mut current_enrollment = current_enrollment.lock().await;
    let current_enrollment = current_enrollment
        .as_mut()
        .filter(|current| same_remote_control_enrollment(current, enrollment))
        .ok_or_else(|| {
            io::Error::other("missing remote control enrollment after websocket auth failure")
        })?;
    if current_enrollment.remote_control_token == enrollment.remote_control_token {
        current_enrollment.clear_server_token();
    }
    Ok(())
}

async fn enroll_and_persist_remote_control_server(
    remote_control_target: &RemoteControlTarget,
    state_db: &StateRuntime,
    auth_context: RemoteControlEnrollmentAuthContext<'_, '_>,
    enrollment: &mut Option<RemoteControlEnrollment>,
    connect_options: RemoteControlConnectOptions<'_>,
    status_publisher: &RemoteControlStatusPublisher,
    selection: RemoteControlEnrollmentSelection,
) -> io::Result<()> {
    match selection {
        RemoteControlEnrollmentSelection::ReuseOrCreate => {
            if enrollment.is_some() {
                return Ok(());
            }
        }
        RemoteControlEnrollmentSelection::ReplaceExisting => {}
    }
    if !connect_options.desired_state_tx.borrow().is_enabled() {
        return Err(io::Error::new(
            ErrorKind::Interrupted,
            "remote control disabled before enrollment",
        ));
    }

    info!(
        "creating new remote control enrollment: websocket_url={}, enroll_url={}, account_id={}",
        remote_control_target.websocket_url,
        remote_control_target.enroll_url,
        auth_context.auth.account_id
    );
    let client = connect_options
        .http_clients
        .client_for_url(&remote_control_target.enroll_url)
        .await
        .map_err(io::Error::other)?;
    let new_enrollment = match enroll_remote_control_server_with_client(
        &client,
        connect_options.request_state,
        remote_control_target,
        auth_context.auth,
        connect_options.installation_id,
        connect_options.server_name,
    )
    .await
    {
        Ok(new_enrollment) => new_enrollment,
        Err(err)
            if err.kind() == ErrorKind::PermissionDenied
                && recover_remote_control_auth(
                    auth_context.recovery.auth_recovery,
                    auth_context.recovery.auth_change_rx,
                    auth_context.auth,
                )
                .await =>
        {
            return Err(state_repaired_error(format!(
                "{err}; retrying after auth recovery"
            )));
        }
        Err(err) => return Err(err),
    };
    let _persistence =
        acquire_persistence_lock(connect_options.desired_state_persistence_lock).await;
    let persistence_preference = match *connect_options.desired_state_tx.borrow() {
        RemoteControlDesiredState::Enabled {
            persistence_preference,
        } => persistence_preference,
        RemoteControlDesiredState::Unknown | RemoteControlDesiredState::Disabled => {
            return Err(io::Error::new(
                ErrorKind::Interrupted,
                "remote control disabled during enrollment",
            ));
        }
    };
    if let Err(err) = update_persisted_remote_control_enrollment(
        Some(state_db),
        remote_control_target,
        &auth_context.auth.account_id,
        connect_options.app_server_client_name,
        Some(&new_enrollment),
        persistence_preference,
    )
    .await
    {
        return Err(io::Error::other(format!(
            "failed to persist remote control enrollment in sqlite state db: {err}"
        )));
    }
    info!(
        "created new remote control enrollment: websocket_url={}, account_id={}, server_id={}, environment_id={}",
        remote_control_target.websocket_url,
        new_enrollment.account_id,
        new_enrollment.server_id,
        new_enrollment.environment_id
    );
    status_publisher.publish_environment_id(Some(new_enrollment.environment_id.clone()));
    *enrollment = Some(new_enrollment);
    Ok(())
}

fn format_remote_control_websocket_connect_error(
    websocket_url: &str,
    err: &tungstenite::Error,
) -> String {
    let mut message =
        format!("failed to connect app-server remote control websocket `{websocket_url}`: {err}");
    let tungstenite::Error::Http(response) = err else {
        return message;
    };

    let _ = write!(message, ", {}", format_headers(response.headers()));
    if let Some(body) = response.body().as_ref()
        && !body.is_empty()
    {
        let body_preview = preview_remote_control_response_body(body);
        let _ = write!(message, ", body: {body_preview}");
    }

    message
}



