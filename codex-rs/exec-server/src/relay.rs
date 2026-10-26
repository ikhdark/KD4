use std::collections::HashMap;
use std::time::Duration;

use codex_exec_server_protocol::JSONRPCMessage;
use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use prost::Message as ProstMessage;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tracing::debug;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

use crate::ExecServerError;
use crate::connection::CHANNEL_CAPACITY;
use crate::connection::JsonRpcConnection;
use crate::connection::JsonRpcConnectionEvent;
use crate::connection::JsonRpcTransport;
use crate::connection::WEBSOCKET_KEEPALIVE_INTERVAL;
use crate::noise_channel::NoiseChannelIdentity;
use crate::noise_channel::NoiseChannelPublicKey;
use crate::noise_channel::PendingResponderHandshake;
use crate::noise_channel::noise_channel_prologue;
use crate::noise_relay::NOISE_RELAY_RESET_REASON;
use crate::noise_relay::executor_stream::ClosedNoiseVirtualStream;
use crate::noise_relay::executor_stream::NoiseVirtualStream;
use crate::noise_relay::executor_stream::spawn_noise_virtual_stream;
use crate::relay_proto::RelayData;
use crate::relay_proto::RelayHandshake;
use crate::relay_proto::RelayMessageFrame;
use crate::relay_proto::RelayReset;
use crate::relay_proto::RelayResume;
use crate::relay_proto::relay_message_frame;
use crate::server::ConnectionProcessor;
use crate::websocket_pong_watchdog::WEBSOCKET_PONG_TIMEOUT;
use crate::websocket_pong_watchdog::WEBSOCKET_PONG_TIMEOUT_REASON;
use crate::websocket_pong_watchdog::WebSocketPongWatchdog;

const RELAY_MESSAGE_FRAME_VERSION: u32 = 1;
const MAX_ACTIVE_NOISE_RELAY_STREAMS: usize = 128;
const MAX_FAILED_NOISE_HANDSHAKES: usize = 8;
const MAX_HARNESS_KEY_AUTHORIZATION_BYTES: usize = 4096;
const MAX_PENDING_HANDSHAKE_VALIDATIONS: usize = 32;
const HARNESS_KEY_VALIDATION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum RendezvousDisconnectReason {
    PeerClose,
    ReadError,
    WriteError,
    PongTimeout,
    HandshakeBudgetExhausted,
    LocalShutdown,
}

impl RendezvousDisconnectReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::PeerClose => "peer_close",
            Self::ReadError => "read_error",
            Self::WriteError => "write_error",
            Self::PongTimeout => WEBSOCKET_PONG_TIMEOUT_REASON,
            Self::HandshakeBudgetExhausted => "handshake_budget_exhausted",
            Self::LocalShutdown => "local_shutdown",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum RelayFrameBodyKind {
    Data,
    Ack,
    Resume,
    Reset,
    Heartbeat,
    Handshake,
}

impl RelayMessageFrame {
    pub(crate) fn data(stream_id: String, seq: u32, payload: Vec<u8>) -> Self {
        Self {
            version: RELAY_MESSAGE_FRAME_VERSION,
            stream_id,
            ack: 0,
            ack_bits: 0,
            body: Some(relay_message_frame::Body::Data(RelayData {
                seq,
                segment_index: 0,
                segment_count: 1,
                payload,
            })),
        }
    }

    pub(crate) fn resume(stream_id: String) -> Self {
        Self {
            version: RELAY_MESSAGE_FRAME_VERSION,
            stream_id,
            ack: 0,
            ack_bits: 0,
            body: Some(relay_message_frame::Body::Resume(RelayResume {
                next_seq: 0,
            })),
        }
    }

    pub(crate) fn handshake(stream_id: String, payload: Vec<u8>) -> Self {
        Self {
            version: RELAY_MESSAGE_FRAME_VERSION,
            stream_id,
            ack: 0,
            ack_bits: 0,
            body: Some(relay_message_frame::Body::Handshake(RelayHandshake {
                payload,
            })),
        }
    }

    pub(crate) fn reset(stream_id: String, reason: String) -> Self {
        Self {
            version: RELAY_MESSAGE_FRAME_VERSION,
            stream_id,
            ack: 0,
            ack_bits: 0,
            body: Some(relay_message_frame::Body::Reset(RelayReset { reason })),
        }
    }

    pub(crate) fn validate(&self) -> Result<RelayFrameBodyKind, ExecServerError> {
        if self.version != RELAY_MESSAGE_FRAME_VERSION {
            return Err(ExecServerError::Protocol(format!(
                "unsupported relay message frame version {}",
                self.version
            )));
        }
        if self.stream_id.trim().is_empty() {
            return Err(ExecServerError::Protocol(
                "relay message frame is missing stream_id".to_string(),
            ));
        }
        match self.body.as_ref() {
            Some(relay_message_frame::Body::Data(data)) => {
                if data.segment_index != 0 || data.segment_count != 1 || data.payload.is_empty() {
                    return Err(ExecServerError::Protocol(
                        "relay data message frame is missing required fields".to_string(),
                    ));
                }
                Ok(RelayFrameBodyKind::Data)
            }
            Some(relay_message_frame::Body::AckFrame(_)) => Ok(RelayFrameBodyKind::Ack),
            Some(relay_message_frame::Body::Resume(_)) => Ok(RelayFrameBodyKind::Resume),
            Some(relay_message_frame::Body::Reset(reset)) => {
                if reset.reason.is_empty() {
                    return Err(ExecServerError::Protocol(
                        "relay reset message frame is missing reason".to_string(),
                    ));
                }
                Ok(RelayFrameBodyKind::Reset)
            }
            Some(relay_message_frame::Body::Heartbeat(_)) => Ok(RelayFrameBodyKind::Heartbeat),
            Some(relay_message_frame::Body::Handshake(handshake)) => {
                if handshake.payload.is_empty() {
                    return Err(ExecServerError::Protocol(
                        "relay handshake message frame is missing payload".to_string(),
                    ));
                }
                Ok(RelayFrameBodyKind::Handshake)
            }
            None => Err(ExecServerError::Protocol(
                "relay message frame is missing body".to_string(),
            )),
        }
    }

    pub(crate) fn into_data(self) -> Result<RelayData, ExecServerError> {
        let kind = self.validate()?;
        if kind != RelayFrameBodyKind::Data {
            return Err(ExecServerError::Protocol(
                "expected relay data message frame".to_string(),
            ));
        }
        match self.body {
            Some(relay_message_frame::Body::Data(data)) => Ok(data),
            _ => Err(ExecServerError::Protocol(
                "expected relay data message frame".to_string(),
            )),
        }
    }

    fn into_jsonrpc_message(self) -> Result<JSONRPCMessage, ExecServerError> {
        let payload = self.into_data()?.payload;
        serde_json::from_slice(&payload).map_err(ExecServerError::Json)
    }

    pub(crate) fn into_handshake_payload(self) -> Result<Vec<u8>, ExecServerError> {
        let kind = self.validate()?;
        if kind != RelayFrameBodyKind::Handshake {
            return Err(ExecServerError::Protocol(
                "expected relay handshake message frame".to_string(),
            ));
        }
        match self.body {
            Some(relay_message_frame::Body::Handshake(handshake)) => Ok(handshake.payload),
            _ => Err(ExecServerError::Protocol(
                "expected relay handshake message frame".to_string(),
            )),
        }
    }

    pub(crate) fn into_reset_reason(self) -> Option<String> {
        match self.body {
            Some(relay_message_frame::Body::Reset(reset)) if !reset.reason.is_empty() => {
                Some(reset.reason)
            }
            _ => None,
        }
    }
}

pub(crate) fn encode_relay_message_frame(frame: &RelayMessageFrame) -> Vec<u8> {
    frame.encode_to_vec()
}

pub(crate) fn decode_relay_message_frame(
    payload: &[u8],
) -> Result<RelayMessageFrame, ExecServerError> {
    RelayMessageFrame::decode(payload)
        .map_err(|err| ExecServerError::Protocol(format!("invalid relay message frame: {err}")))
}

pub(crate) fn jsonrpc_payload(message: &JSONRPCMessage) -> Result<Vec<u8>, ExecServerError> {
    serde_json::to_vec(message).map_err(ExecServerError::Json)
}

pub(crate) fn harness_connection_from_websocket<T, E>(
    stream: T,
    connection_label: String,
) -> JsonRpcConnection
where
    T: Sink<Message, Error = E> + Stream<Item = Result<Message, E>> + Unpin + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let stream_id = Uuid::new_v4().to_string();
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (incoming_tx, incoming_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (disconnected_tx, disconnected_rx) = watch::channel(false);
    let websocket_task = tokio::spawn(async move {
        let budget = crate::connection::WEBSOCKET_MESSAGE_TIMEOUT;
        let (mut writer, mut reader) = stream.split();
        let resume = RelayMessageFrame::resume(stream_id.clone());
        if !matches!(
            timeout(
                budget,
                writer.send(Message::Binary(encode_relay_message_frame(&resume).into()))
            )
            .await,
            Ok(Ok(()))
        ) {
            let _ = disconnected_tx.send(true);
            return;
        }
        let liveness = crate::websocket_pong_watchdog::WebSocketLiveness::new(budget);
        // Frames read ahead of a busy consumer live outside `read`: a reset,
        // close, or liveness verdict must not drop already received messages.
        let mut pending: std::collections::VecDeque<JsonRpcConnectionEvent> =
            std::collections::VecDeque::new();
        let read = async {
            loop {
                let incoming = tokio::select! {
                    permit = incoming_tx.reserve(), if !pending.is_empty() => {
                        let Ok(permit) = permit else { return None; };
                        if let Some(event) = pending.pop_front() { permit.send(event); }
                        continue;
                    }
                    incoming = reader.next(), if pending.len() < crate::connection::WEBSOCKET_CONTROL_BACKLOG => incoming,
                };
                match incoming {
                    Some(Ok(Message::Binary(payload))) => {
                        let frame = match decode_relay_message_frame(payload.as_ref()) {
                            Ok(frame) => frame,
                            Err(error) => {
                                pending.push_back(JsonRpcConnectionEvent::MalformedMessage {
                                    reason: error.to_string(),
                                });
                                continue;
                            }
                        };
                        if frame.stream_id != stream_id {
                            continue;
                        }
                        match frame.validate() {
                            Ok(RelayFrameBodyKind::Data) => match frame.into_jsonrpc_message() {
                                Ok(message) => {
                                    pending.push_back(JsonRpcConnectionEvent::Message(message))
                                }
                                Err(error) => {
                                    pending.push_back(JsonRpcConnectionEvent::MalformedMessage {
                                        reason: error.to_string(),
                                    })
                                }
                            },
                            Ok(RelayFrameBodyKind::Reset) => return frame.into_reset_reason(),
                            Ok(_) => {}
                            Err(error) => {
                                pending.push_back(JsonRpcConnectionEvent::MalformedMessage {
                                    reason: error.to_string(),
                                })
                            }
                        }
                    }
                    Some(Ok(Message::Pong(_))) => liveness.received_pong(),
                    Some(Ok(Message::Ping(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Text(_))) => {
                        pending.push_back(JsonRpcConnectionEvent::MalformedMessage {
                            reason: "relay exec-server transport expects binary protobuf frames"
                                .into(),
                        })
                    }
                    Some(Ok(Message::Close(_))) | None => return None,
                    Some(Err(error)) => {
                        return Some(format!(
                            "failed to read relay websocket from {connection_label}: {error}"
                        ));
                    }
                }
            }
        };
        let write = async {
            let mut keepalive = tokio::time::interval_at(
                tokio::time::Instant::now() + WEBSOCKET_KEEPALIVE_INTERVAL,
                WEBSOCKET_KEEPALIVE_INTERVAL,
            );
            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut next_seq = 0u32;
            loop {
                tokio::select! {
                    message = outgoing_rx.recv() => {
                        let message = message?;
                        let payload = match jsonrpc_payload(&message) {
                            Ok(payload) => payload,
                            Err(error) => return Some(error.to_string()),
                        };
                        let frame = RelayMessageFrame::data(stream_id.clone(), next_seq, payload);
                        next_seq = next_seq.wrapping_add(1);
                        match timeout(budget, writer.send(Message::Binary(encode_relay_message_frame(&frame).into()))).await {
                            Ok(Ok(())) => {},
                            Ok(Err(error)) => return Some(error.to_string()),
                            Err(_) => return Some("relay websocket write deadline expired".into()),
                        }
                    }
                    _ = keepalive.tick() => {
                        let generation = liveness.pong_generation();
                        match timeout(budget, writer.send(Message::Ping(Vec::new().into()))).await {
                            Ok(Ok(())) => liveness.ping_flushed(generation),
                            Ok(Err(error)) => return Some(error.to_string()),
                            Err(_) => return Some("relay websocket ping write deadline expired".into()),
                        }
                    }
                }
            }
        };
        // Retain sends across inbound activity and keep controls independent of
        // application delivery. A stopped consumer is bounded by the watchdog.
        let reason = tokio::select! {
            biased;
            reason = read => reason,
            reason = write => reason,
            _ = liveness.expired() => Some("relay websocket Pong deadline expired".into()),
        };
        let _ = disconnected_tx.send(true);
        // Deliver received frames in order ahead of the terminal event.
        for event in pending.drain(..) {
            if incoming_tx.send(event).await.is_err() {
                break;
            }
        }
        let _ = incoming_tx
            .send(JsonRpcConnectionEvent::Disconnected { reason })
            .await;
    });
    JsonRpcConnection {
        outgoing_tx,
        incoming_rx,
        disconnected_rx,
        task_handles: vec![websocket_task],
        transport: JsonRpcTransport::Plain,
    }
}

/// Validates that a Noise-authenticated harness public key is authorized.
///
/// Implementations must consult an authority independent of rendezvous. The
/// exec-server invokes this after parsing the first IK message and before
/// completing the responder handshake.
pub(crate) trait HarnessKeyValidator: Send + Sync {
    fn validate_harness_key(
        &self,
        harness_public_key: &NoiseChannelPublicKey,
        authorization: &str,
    ) -> impl std::future::Future<Output = Result<(), ExecServerError>> + Send;
}

/// Serve authenticated virtual JSON-RPC streams over one executor websocket.
///
/// Parsing the first Noise message authenticates the harness key. Only a
/// successful registry check turns that pending handshake into a virtual stream.
#[tracing::instrument(level = "debug", skip_all, fields(noise_side = "executor"))]
pub(crate) async fn run_multiplexed_environment<S, V>(
    stream: WebSocketStream<S>,
    processor: ConnectionProcessor,
    environment_id: String,
    executor_registration_id: String,
    identity: NoiseChannelIdentity,
    validator: V,
) -> RendezvousDisconnectReason
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    V: HarnessKeyValidator + Clone + 'static,
{
    debug!(
        environment_id,
        executor_registration_id, "Noise executor relay details"
    );
    let (mut websocket_sink, mut websocket_stream) = stream.split();
    let (physical_outgoing_tx, mut physical_outgoing_rx) =
        mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    let (closed_stream_tx, mut closed_stream_rx) =
        mpsc::channel::<ClosedNoiseVirtualStream>(MAX_ACTIVE_NOISE_RELAY_STREAMS);
    let (pong_tx, mut pong_rx) = mpsc::channel(1);
    // Use a separate writer so this loop never waits on the channel it drains.
    let mut physical_writer_task = tokio::spawn(async move {
        let mut keepalive = tokio::time::interval_at(
            tokio::time::Instant::now() + WEBSOCKET_KEEPALIVE_INTERVAL,
            WEBSOCKET_KEEPALIVE_INTERVAL,
        );
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut pong_watchdog = WebSocketPongWatchdog::new(WEBSOCKET_PONG_TIMEOUT);
        let pong_deadline = tokio::time::sleep(WEBSOCKET_PONG_TIMEOUT);
        tokio::pin!(pong_deadline);
        loop {
            let message = tokio::select! {
                pong = pong_rx.recv() => {
                    let Some(()) = pong else {
                        break RendezvousDisconnectReason::LocalShutdown;
                    };
                    pong_watchdog.received_pong();
                    continue;
                }
                _ = &mut pong_deadline, if pong_watchdog.deadline().is_some() => {
                    match pong_rx.try_recv() {
                        Ok(()) => {
                            pong_watchdog.received_pong();
                            continue;
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                            break RendezvousDisconnectReason::PongTimeout;
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                            break RendezvousDisconnectReason::LocalShutdown;
                        }
                    }
                }
                _ = keepalive.tick(), if pong_watchdog.deadline().is_none() => {
                    Message::Ping(Vec::new().into())
                }
                encoded = physical_outgoing_rx.recv() => {
                    let Some(encoded) = encoded else {
                        break RendezvousDisconnectReason::LocalShutdown;
                    };
                    Message::Binary(encoded.into())
                }
            };
            let is_keepalive_ping = matches!(message, Message::Ping(_));
            let write_deadline = pong_watchdog.write_deadline(tokio::time::Instant::now());
            match tokio::time::timeout_at(write_deadline, websocket_sink.send(message)).await {
                Ok(Ok(())) => {
                    if is_keepalive_ping {
                        pong_watchdog.ping_sent(tokio::time::Instant::now());
                        if let Some(deadline) = pong_watchdog.deadline() {
                            pong_deadline.as_mut().reset(deadline);
                        }
                    }
                }
                Ok(Err(error)) => {
                    warn!("Noise multiplexed environment websocket write failed: {error}");
                    break RendezvousDisconnectReason::WriteError;
                }
                Err(_) => {
                    warn!("Noise multiplexed environment websocket write timed out");
                    break RendezvousDisconnectReason::WriteError;
                }
            }
        }
    });
    let mut streams: HashMap<String, NoiseVirtualStream> = HashMap::new();
    let mut pending_handshakes: HashMap<String, PendingHandshake> = HashMap::new();
    let mut validation_tasks: JoinSet<HarnessKeyValidationResult> = JoinSet::new();
    let mut failed_handshakes = 0usize;
    let mut next_validation_id = 0u64;
    let disconnect_reason;

    loop {
        // Expire against the current map entry, so no stale timer can close a reused ID.
        let now = tokio::time::Instant::now();
        let expired = streams
            .iter()
            .filter(|(_, stream)| {
                stream
                    .gap_deadline()
                    .is_some_and(|deadline| deadline <= now)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            if let Some(stream) = streams.remove(&id) {
                stream.disconnect(Some(
                    crate::noise_relay::ordered_ciphertext::CIPHERTEXT_GAP_TIMEOUT_REASON.into(),
                ));
                send_reset(&physical_outgoing_tx, id);
            }
        }
        let gap_deadline = streams
            .values()
            .filter_map(NoiseVirtualStream::gap_deadline)
            .min();
        // Registry calls run separately so a slow check does not block the relay.
        let frame = tokio::select! {
            _ = processor.wait_for_shutdown() => {
                disconnect_reason = RendezvousDisconnectReason::LocalShutdown;
                break;
            }
            _ = async {
                match gap_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => continue,
            writer_result = &mut physical_writer_task => {
                match writer_result {
                    Ok(reason) => disconnect_reason = reason,
                    Err(error) => {
                        warn!("Noise multiplexed environment websocket writer failed: {error}");
                        disconnect_reason = RendezvousDisconnectReason::LocalShutdown;
                    }
                }
                break;
            }
            Some(closed_stream) = closed_stream_rx.recv() => {
                // A stream ID may have been reused before this writer exits.
                // Remove only the instance that sent the notification.
                let is_current = streams
                    .get(&closed_stream.stream_id)
                    .is_some_and(|stream| stream.instance_id == closed_stream.instance_id);
                if is_current {
                    streams.remove(&closed_stream.stream_id);
                }
                continue;
            }
            validation_result = validation_tasks.join_next(), if !validation_tasks.is_empty() => {
                match validation_result {
                    Some(Ok(validation_result)) => {
                        // The stream ID may have been reused while validation ran.
                        let is_current = pending_handshakes
                            .get(&validation_result.stream_id)
                            .is_some_and(|pending| {
                                pending.validation_id == validation_result.validation_id
                            });
                        if !is_current {
                            continue;
                        }
                        let Some(pending) =
                            pending_handshakes.remove(&validation_result.stream_id)
                        else {
                            continue;
                        };
                        if validation_result.result.is_err() {
                            // Validator errors may contain authorization details.
                            warn!(
                                noise_event = "authorization",
                                noise_outcome = "error",
                                noise_reason = "authorization_failed",
                                "Noise harness authorization failed"
                            );
                            debug!(
                                stream_id = validation_result.stream_id,
                                "Noise harness authorization failure details"
                            );
                            send_reset(&physical_outgoing_tx, validation_result.stream_id);
                            if failed_handshake_budget_exhausted(&mut failed_handshakes) {
                                disconnect_reason = RendezvousDisconnectReason::HandshakeBudgetExhausted;
                                warn!("closing Noise relay after repeated handshake failures");
                                break;
                            }
                            continue;
                        }
                        if streams.len() >= MAX_ACTIVE_NOISE_RELAY_STREAMS {
                            warn!("Noise relay has too many active streams");
                            send_reset(&physical_outgoing_tx, validation_result.stream_id);
                            continue;
                        }

                        // This is the only point where the responder completes
                        // IK and exposes a JSON-RPC stream: Noise authenticated
                        // the harness key and the registry authorized it.
                        let (transport, response) = match pending.handshake.complete() {
                            Ok(completed) => completed,
                            Err(error) => {
                                warn!("failed to complete Noise relay handshake: {error}");
                                send_reset(&physical_outgoing_tx, validation_result.stream_id);
                                if failed_handshake_budget_exhausted(&mut failed_handshakes) {
                                    disconnect_reason = RendezvousDisconnectReason::HandshakeBudgetExhausted;
                                    warn!("closing Noise relay after repeated handshake failures");
                                    break;
                                }
                                continue;
                            }
                        };
                        let response = RelayMessageFrame::handshake(
                            validation_result.stream_id.clone(),
                            response,
                        );
                        // Admission reserved this capacity before cryptography and
                        // authorization. Established traffic cannot discard the reply.
                        pending.reply_permit.send(encode_relay_message_frame(&response));
                        info!(
                            noise_event = "handshake",
                            noise_outcome = "ok",
                            "Noise executor handshake completed"
                        );
                        debug!(
                            stream_id = validation_result.stream_id,
                            active_streams = streams.len() + 1,
                            "Noise executor stream activated"
                        );
                        streams.insert(
                            validation_result.stream_id.clone(),
                            spawn_noise_virtual_stream(
                                validation_result.stream_id,
                                validation_result.validation_id,
                                processor.clone(),
                                physical_outgoing_tx.clone(),
                                closed_stream_tx.clone(),
                                transport,
                            ),
                        );
                    }
                    Some(Err(error)) => {
                        warn!("Noise relay harness key validation task failed: {error}");
                        let stream_ids = pending_handshakes.keys().cloned().collect::<Vec<_>>();
                        pending_handshakes.clear();
                        for stream_id in stream_ids {
                            send_reset(&physical_outgoing_tx, stream_id);
                        }
                    }
                    None => {}
                }
                continue;
            }
            incoming_message = websocket_stream.next() => match incoming_message {
                Some(Ok(Message::Binary(payload))) => match decode_relay_message_frame(payload.as_ref()) {
                    Ok(frame) => frame,
                    Err(error) => {
                        warn!("dropping malformed Noise relay frame from harness: {error}");
                        continue;
                    }
                },
                Some(Ok(Message::Close(_))) | None => {
                    disconnect_reason = RendezvousDisconnectReason::PeerClose;
                    break;
                }
                Some(Ok(Message::Pong(_))) => {
                    let _ = pong_tx.try_send(());
                    continue;
                }
                Some(Ok(Message::Ping(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Text(_))) => {
                    warn!("dropping non-binary Noise relay frame from harness");
                    continue;
                }
                Some(Err(error)) => {
                    debug!("Noise multiplexed environment websocket read failed: {error}");
                    disconnect_reason = RendezvousDisconnectReason::ReadError;
                    break;
                }
            }
        };

        let kind = match frame.validate() {
            Ok(kind) => kind,
            Err(error) => {
                warn!("dropping invalid Noise relay frame: {error}");
                continue;
            }
        };
        let stream_id = frame.stream_id.clone();
        match kind {
            RelayFrameBodyKind::Handshake => {
                // Reject duplicate or busy streams before paying for a hybrid
                // handshake. Malformed attempts that reach cryptography are
                // covered by the connection-wide failure budget below.
                if streams.contains_key(&stream_id) {
                    send_reset(&physical_outgoing_tx, stream_id);
                    continue;
                }
                // Removing pending state makes the in-flight validation result stale.
                if pending_handshakes.remove(&stream_id).is_some() {
                    send_reset(&physical_outgoing_tx, stream_id);
                    if failed_handshake_budget_exhausted(&mut failed_handshakes) {
                        disconnect_reason = RendezvousDisconnectReason::HandshakeBudgetExhausted;
                        warn!("closing Noise relay after repeated handshake failures");
                        break;
                    }
                    continue;
                }
                if streams.len() >= MAX_ACTIVE_NOISE_RELAY_STREAMS {
                    warn!("Noise relay has too many active streams");
                    send_reset(&physical_outgoing_tx, stream_id);
                    continue;
                }
                if validation_tasks.len() >= MAX_PENDING_HANDSHAKE_VALIDATIONS {
                    warn!("Noise relay has too many pending harness key validations");
                    send_reset(&physical_outgoing_tx, stream_id);
                    continue;
                }
                let prologue =
                    noise_channel_prologue(&environment_id, &executor_registration_id, &stream_id);
                let Ok(reply_permit) = physical_outgoing_tx.clone().try_reserve_owned() else {
                    send_reset(&physical_outgoing_tx, stream_id);
                    continue;
                };
                let request = match frame.into_handshake_payload() {
                    Ok(request) => request,
                    Err(error) => {
                        warn!("failed to read Noise relay handshake frame: {error}");
                        send_reset(&physical_outgoing_tx, stream_id);
                        continue;
                    }
                };
                let mut pending =
                    match PendingResponderHandshake::read_request(&identity, &prologue, &request) {
                        Ok(pending) => pending,
                        Err(error) => {
                            warn!("failed to read Noise relay handshake request: {error}");
                            send_reset(&physical_outgoing_tx, stream_id);
                            if failed_handshake_budget_exhausted(&mut failed_handshakes) {
                                disconnect_reason =
                                    RendezvousDisconnectReason::HandshakeBudgetExhausted;
                                warn!("closing Noise relay after repeated handshake failures");
                                break;
                            }
                            continue;
                        }
                    };

                // The authorization and authenticated harness key come from the
                // same encrypted IK message and are validated together.
                let authorization = match String::from_utf8(std::mem::take(&mut pending.payload)) {
                    Ok(authorization)
                        if authorization.len() <= MAX_HARNESS_KEY_AUTHORIZATION_BYTES =>
                    {
                        Some(authorization)
                    }
                    Ok(_) => {
                        warn!("Noise relay handshake authorization is too long");
                        None
                    }
                    Err(_) => {
                        warn!("Noise relay handshake authorization is not UTF-8");
                        None
                    }
                };
                let Some(authorization) = authorization else {
                    send_reset(&physical_outgoing_tx, stream_id);
                    if failed_handshake_budget_exhausted(&mut failed_handshakes) {
                        disconnect_reason = RendezvousDisconnectReason::HandshakeBudgetExhausted;
                        warn!("closing Noise relay after repeated handshake failures");
                        break;
                    }
                    continue;
                };
                let harness_public_key = pending.initiator_public_key.clone();
                let validation_id = next_validation_id;
                next_validation_id += 1;
                let validation_cancelled = tokio_util::sync::CancellationToken::new();
                pending_handshakes.insert(
                    stream_id.clone(),
                    PendingHandshake {
                        validation_id,
                        handshake: pending,
                        reply_permit,
                        _cancel_validation_on_drop: validation_cancelled.clone().drop_guard(),
                    },
                );
                let validator = validator.clone();

                // Failed validation leaves no transport state and sends only a
                // generic reset.
                validation_tasks.spawn(async move {
                    let result = tokio::select! {
                        biased;
                        _ = validation_cancelled.cancelled() => Err(ExecServerError::Protocol(
                            "Noise relay harness validation cancelled".to_string(),
                        )),
                        result = timeout(HARNESS_KEY_VALIDATION_TIMEOUT,
                            validator.validate_harness_key(&harness_public_key, &authorization)) => match result {
                            Ok(result) => result,
                            Err(_) => Err(ExecServerError::Protocol(
                                "timed out validating Noise relay harness key".to_string(),
                            )),
                        },
                    };
                    HarnessKeyValidationResult {
                        stream_id,
                        validation_id,
                        result,
                    }
                });
            }
            RelayFrameBodyKind::Data => {
                // Removing pending state also makes any in-flight validation stale.
                let Some(stream) = streams.get_mut(&stream_id) else {
                    let canceled_pending_handshake =
                        pending_handshakes.remove(&stream_id).is_some();
                    send_reset(&physical_outgoing_tx, stream_id);
                    if canceled_pending_handshake
                        && failed_handshake_budget_exhausted(&mut failed_handshakes)
                    {
                        disconnect_reason = RendezvousDisconnectReason::HandshakeBudgetExhausted;
                        warn!("closing Noise relay after repeated handshake failures");
                        break;
                    }
                    continue;
                };
                let data = match frame.into_data() {
                    Ok(data) => data,
                    Err(error) => {
                        warn!("dropping malformed Noise relay data frame: {error}");
                        streams.remove(&stream_id);
                        send_reset(&physical_outgoing_tx, stream_id);
                        continue;
                    }
                };
                if let Err(error) = stream.receive_data(data) {
                    warn!("failed to process Noise relay payload: {error}");
                    streams.remove(&stream_id);
                    send_reset(&physical_outgoing_tx, stream_id);
                }
            }
            RelayFrameBodyKind::Reset => {
                if pending_handshakes.remove(&stream_id).is_some()
                    && failed_handshake_budget_exhausted(&mut failed_handshakes)
                {
                    disconnect_reason = RendezvousDisconnectReason::HandshakeBudgetExhausted;
                    break;
                }
                if let Some(stream) = streams.remove(&stream_id) {
                    // The reset reason is unauthenticated, so do not log it.
                    stream.disconnect(/*reason*/ None);
                }
            }
            RelayFrameBodyKind::Ack
            | RelayFrameBodyKind::Resume
            | RelayFrameBodyKind::Heartbeat => {}
        }
    }

    for (_stream_id, stream) in streams {
        stream.disconnect(/*reason*/ None);
    }
    // Dropping the JoinSet aborts any registry checks still running.
    if !physical_writer_task.is_finished() {
        physical_writer_task.abort();
        let _ = physical_writer_task.await;
    }
    disconnect_reason
}

/// Charge one failed authenticated-channel attempt to this physical relay.
///
/// Closing after a small fixed budget prevents a peer that has not been
/// authorized from triggering unbounded hybrid handshakes or registry checks.
fn failed_handshake_budget_exhausted(failed_handshakes: &mut usize) -> bool {
    *failed_handshakes += 1;
    *failed_handshakes >= MAX_FAILED_NOISE_HANDSHAKES
}

/// Responder state held while registry authorization is pending.
struct PendingHandshake {
    validation_id: u64,
    handshake: PendingResponderHandshake,
    reply_permit: mpsc::OwnedPermit<Vec<u8>>,
    _cancel_validation_on_drop: tokio_util::sync::DropGuard,
}

/// `validation_id` prevents an old check from completing a reused `stream_id`.
struct HarnessKeyValidationResult {
    stream_id: String,
    validation_id: u64,
    result: Result<(), ExecServerError>,
}

/// Queue a best-effort reset without blocking the shared websocket loop.
/// Reset reasons are relay control data and are not treated as trusted text.
fn send_reset(physical_outgoing_tx: &mpsc::Sender<Vec<u8>>, stream_id: String) {
    let reset = RelayMessageFrame::reset(stream_id, NOISE_RELAY_RESET_REASON.to_string());
    let _ = physical_outgoing_tx.try_send(encode_relay_message_frame(&reset));
}


