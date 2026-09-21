//! Harness side of the Noise relay.
//!
//! The rendezvous service routes frames by `stream_id`, but does not authenticate
//! the executor or see JSON-RPC plaintext. We claim a stream, complete hybrid IK
//! against the registry-provided executor key, and then expose the result as a
//! normal `JsonRpcConnection`. Outbound JSON-RPC is framed and split into Noise
//! records; inbound records are reordered before decryption and reassembly.

use futures::FutureExt;
use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tracing::Instrument;
use tracing::debug;
use tracing::info;
use uuid::Uuid;

use crate::ExecServerError;
use crate::connection::CHANNEL_CAPACITY;
use crate::connection::JsonRpcConnection;
use crate::connection::JsonRpcConnectionEvent;
use crate::connection::JsonRpcTransport;
use crate::connection::WEBSOCKET_KEEPALIVE_INTERVAL;
use crate::noise_channel::InitiatorHandshake;
use crate::noise_channel::NoiseChannelIdentity;
use crate::noise_channel::NoiseChannelPublicKey;
use crate::noise_channel::noise_channel_prologue;
use crate::noise_relay::message_framing::JsonRpcMessageDecoder;
use crate::noise_relay::message_framing::NOISE_RECORD_PLAINTEXT_LEN;
use crate::noise_relay::message_framing::frame_jsonrpc_message;
use crate::noise_relay::ordered_ciphertext::OrderedCiphertextFrames;
use crate::noise_relay::take_next_sequence;
use crate::relay::RelayFrameBodyKind;
use crate::relay::decode_relay_message_frame;
use crate::relay::encode_relay_message_frame;
use crate::relay_proto::RelayMessageFrame;
use crate::websocket_pong_watchdog::WEBSOCKET_PONG_TIMEOUT;
use crate::websocket_pong_watchdog::WEBSOCKET_PONG_TIMEOUT_REASON;
use crate::websocket_pong_watchdog::WebSocketPongWatchdog;

/// Values that bind one harness websocket to the intended executor registration.
///
/// These fields all come from the same registry response. Keeping them together
/// makes that relationship visible at the call site and avoids mixing up the
/// several string and key arguments used to start the handshake.
pub(crate) struct NoiseHarnessConnectionArgs {
    pub(crate) connection_label: String,
    pub(crate) environment_id: String,
    pub(crate) executor_registration_id: String,
    pub(crate) identity: NoiseChannelIdentity,
    pub(crate) responder_public_key: NoiseChannelPublicKey,
    pub(crate) harness_key_authorization: String,
}

// Reset frames are cleartext relay control and are not authenticated by Noise.
// Preserve the availability signal while replacing attacker-controlled reason
// text before it reaches disconnect diagnostics.
const NOISE_RELAY_RESET_DISCONNECT_REASON: &str = "Noise relay stream reset";
// Give a Pong already queued behind data a bounded chance to reach the reader.
const MAX_FRAMES_DRAINED_AFTER_PONG_DEADLINE: usize = 32;

/// Adapt one harness rendezvous websocket into an authenticated JSON-RPC connection.
///
/// The returned connection is not usable until the background task completes
/// hybrid IK against the registry-pinned exec-server key. Rendezvous can see
/// stream metadata and ciphertext, but never JSON-RPC plaintext or either
/// endpoint's private key. Failures close the connection rather than falling
/// back to plaintext.
pub(crate) fn noise_harness_connection_from_websocket<T, E>(
    stream: T,
    args: NoiseHarnessConnectionArgs,
) -> JsonRpcConnection
where
    T: Sink<Message, Error = E> + Stream<Item = Result<Message, E>> + Unpin + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let NoiseHarnessConnectionArgs {
        connection_label,
        environment_id,
        executor_registration_id,
        identity,
        responder_public_key,
        harness_key_authorization,
    } = args;
    let stream_id = Uuid::new_v4().to_string();
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (incoming_tx, incoming_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (disconnected_tx, disconnected_rx) = watch::channel(false);
    let stream_span = tracing::debug_span!("noise_relay.stream", noise_side = "harness",);
    debug!(
        environment_id,
        executor_registration_id, stream_id, "Noise harness relay details"
    );

    let websocket_task = tokio::spawn(async move {
        let mut websocket = stream;

        // Bind the Noise transcript to the exact environment registration and
        // virtual relay stream before emitting any handshake bytes. A captured
        // handshake cannot be spliced onto a different routed connection.
        let prologue =
            noise_channel_prologue(&environment_id, &executor_registration_id, &stream_id);
        let (initiator_handshake, request) = match InitiatorHandshake::start(
            &identity,
            &responder_public_key,
            &prologue,
            harness_key_authorization.as_bytes(),
        ) {
            Ok(handshake) => handshake,
            Err(error) => {
                send_disconnected(
                    &incoming_tx,
                    &disconnected_tx,
                    format!("failed to start Noise relay handshake: {error}"),
                );
                return;
            }
        };

        // Resume claims the stream ID at rendezvous; Handshake carries the
        // opaque first IK message. No JSON-RPC data is sent before the
        // responder proves possession of the pinned static key.
        let resume = RelayMessageFrame::resume(stream_id.clone());
        let handshake = RelayMessageFrame::handshake(stream_id.clone(), request);
        if websocket
            .send(Message::Binary(encode_relay_message_frame(&resume).into()))
            .await
            .is_err()
            || websocket
                .send(Message::Binary(
                    encode_relay_message_frame(&handshake).into(),
                ))
                .await
                .is_err()
        {
            let _ = disconnected_tx.send(true);
            return;
        }

        // During the handshake, ignore unrelated routed streams and control
        // frames, but reject data on our stream. Accepting early data would
        // create a plaintext or unauthenticated application path.
        let mut transport = loop {
            let Some(incoming_message) = websocket.next().await else {
                send_disconnected(
                    &incoming_tx,
                    &disconnected_tx,
                    "Noise relay websocket ended during handshake".to_string(),
                );
                return;
            };
            let message = match incoming_message {
                Ok(Message::Binary(payload)) => payload,
                Ok(Message::Close(_)) => {
                    send_disconnected(
                        &incoming_tx,
                        &disconnected_tx,
                        "Noise relay websocket received close frame during handshake".to_string(),
                    );
                    return;
                }
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => continue,
                Ok(Message::Text(_)) => {
                    send_disconnected(
                        &incoming_tx,
                        &disconnected_tx,
                        "Noise relay transport expects binary protobuf frames".to_string(),
                    );
                    return;
                }
                Err(error) => {
                    send_disconnected(
                        &incoming_tx,
                        &disconnected_tx,
                        format!(
                            "failed to read Noise relay websocket from {connection_label}: {error}"
                        ),
                    );
                    return;
                }
            };
            let frame = match decode_relay_message_frame(message.as_ref()) {
                Ok(frame) => frame,
                Err(error) => {
                    send_disconnected(
                        &incoming_tx,
                        &disconnected_tx,
                        format!("failed to parse Noise relay frame: {error}"),
                    );
                    return;
                }
            };
            if frame.stream_id != stream_id {
                debug!("Noise relay ignored frame for unrelated stream during handshake");
                continue;
            }
            match frame.validate() {
                Ok(RelayFrameBodyKind::Handshake) => {
                    let response = match frame.into_handshake_payload() {
                        Ok(response) => response,
                        Err(error) => {
                            send_disconnected(
                                &incoming_tx,
                                &disconnected_tx,
                                format!("invalid Noise relay handshake response: {error}"),
                            );
                            return;
                        }
                    };
                    match initiator_handshake.finish(&response) {
                        Ok(transport) => {
                            info!(
                                noise_event = "handshake",
                                noise_outcome = "ok",
                                "Noise harness handshake completed"
                            );
                            break transport;
                        }
                        Err(error) => {
                            send_disconnected(
                                &incoming_tx,
                                &disconnected_tx,
                                format!("Noise relay handshake failed: {error}"),
                            );
                            return;
                        }
                    }
                }
                Ok(RelayFrameBodyKind::Reset) => {
                    send_disconnected(
                        &incoming_tx,
                        &disconnected_tx,
                        NOISE_RELAY_RESET_DISCONNECT_REASON.to_string(),
                    );
                    return;
                }
                Ok(
                    RelayFrameBodyKind::Ack
                    | RelayFrameBodyKind::Resume
                    | RelayFrameBodyKind::Heartbeat,
                ) => {}
                Ok(RelayFrameBodyKind::Data) | Err(_) => {
                    send_disconnected(
                        &incoming_tx,
                        &disconnected_tx,
                        "Noise relay received data before handshake completion".to_string(),
                    );
                    return;
                }
            }
        };

        // Keep socket reads, the single ordered write, and application delivery
        // independently pollable in one owner. Noise state never crosses an await.
        let (sink, reader) = websocket.split();
        let mut sink = Some(sink);
        let mut reader = reader.peekable();
        let mut writing = None;
        let mut ping_in_flight = false;
        let mut pong_during_ping = false;
        let mut next_outbound_seq = 0u32;
        let mut inbound_ciphertexts = OrderedCiphertextFrames::default();
        let mut inbound_decoder = JsonRpcMessageDecoder::default();
        let mut keepalive = tokio::time::interval_at(
            tokio::time::Instant::now() + WEBSOCKET_KEEPALIVE_INTERVAL,
            WEBSOCKET_KEEPALIVE_INTERVAL,
        );
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut pong_watchdog = WebSocketPongWatchdog::new(WEBSOCKET_PONG_TIMEOUT);
        let mut pending_outbound: Option<(Vec<u8>, usize)> = None;
        let mut delivery = std::collections::VecDeque::new();
        let mut delivery_deadline = None;
        let mut frames_drained_after_pong_deadline = 0usize;
        'relay: loop {
            let now = tokio::time::Instant::now();
            let gap_deadline = inbound_ciphertexts.gap_deadline();
            if gap_deadline.is_some_and(|deadline| deadline <= now) {
                send_disconnected(&incoming_tx, &disconnected_tx,
                    super::ordered_ciphertext::CIPHERTEXT_GAP_TIMEOUT_REASON.into());
                return;
            }
            if delivery_deadline.is_some_and(|deadline| deadline <= now) {
                send_disconnected(&incoming_tx, &disconnected_tx, "application_backpressure".into());
                return;
            }
            let pong_expired = pong_watchdog.deadline().is_some_and(|deadline| deadline <= now);
            if pong_expired && (frames_drained_after_pong_deadline >= MAX_FRAMES_DRAINED_AFTER_PONG_DEADLINE
                || std::pin::Pin::new(&mut reader).peek().now_or_never().is_none()) {
                send_disconnected(&incoming_tx, &disconnected_tx, WEBSOCKET_PONG_TIMEOUT_REASON.into());
                return;
            }
            // Give due keepalives priority over another data fragment.
            if sink.is_some() && pong_watchdog.deadline().is_none() && keepalive.tick().now_or_never().is_some() {
                writing = sink.take().map(|sink| Box::pin(write_owned(sink, Message::Ping(Vec::new().into()), true)));
                ping_in_flight = true;
                pong_during_ping = false;
            }
            let next_deadline = [gap_deadline, delivery_deadline, pong_watchdog.deadline()]
                .into_iter().flatten().min();
            tokio::select! {
                _ = async {
                    match next_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                }, if !pong_expired => {}
                result = async {
                    match writing.as_mut() {
                        Some(write) => write.await,
                        None => std::future::pending().await,
                    }
                }, if writing.is_some() => {
                    let (returned_sink, result, ping) = result;
                    writing = None;
                    sink = Some(returned_sink);
                    if let Err(reason) = result {
                        send_disconnected(&incoming_tx, &disconnected_tx, reason);
                        return;
                    }
                    if ping {
                        // The peer's response clock starts only after the Ping flushes.
                        ping_in_flight = false;
                        if !pong_during_ping { pong_watchdog.ping_sent(tokio::time::Instant::now()); }
                        frames_drained_after_pong_deadline = 0;
                    }
                }
                permit = incoming_tx.reserve(), if !delivery.is_empty() => {
                    let Ok(permit) = permit else { break; };
                    if let Some(event) = delivery.pop_front() { permit.send(event); }
                    delivery_deadline = (!delivery.is_empty()).then(|| tokio::time::Instant::now() + WEBSOCKET_PONG_TIMEOUT);
                }
                _ = keepalive.tick(), if sink.is_some() && pong_watchdog.deadline().is_none() => {
                    writing = sink.take().map(|sink| Box::pin(write_owned(sink, Message::Ping(Vec::new().into()), true)));
                ping_in_flight = true;
                pong_during_ping = false;
                }
                message = outgoing_rx.recv(), if pending_outbound.is_none() && !pong_expired => {
                    let Some(message) = message else { break; };
                    pending_outbound = Some(match frame_jsonrpc_message(&message) {
                        Ok(frame) => (frame, 0),
                        Err(error) => { send_malformed(&incoming_tx, error.to_string()); break; }
                    });
                }
                _ = std::future::ready(()), if pending_outbound.is_some() && sink.is_some() && !pong_expired => {
                    let seq = match take_next_sequence(&mut next_outbound_seq) {
                        Ok(seq) => seq,
                        Err(error) => { send_malformed(&incoming_tx, error.to_string()); break; }
                    };
                    let Some((frame, offset)) = pending_outbound.as_mut() else { continue; };
                    let end = (*offset + NOISE_RECORD_PLAINTEXT_LEN).min(frame.len());
                    let encrypted = match transport.encrypt(&frame[*offset..end]) {
                        Ok(encrypted) => encrypted,
                        Err(error) => { send_malformed(&incoming_tx, error.to_string()); break; }
                    };
                    *offset = end;
                    if end == frame.len() { pending_outbound = None; }
                    let frame = RelayMessageFrame::data(stream_id.clone(), seq, encrypted);
                    let message = Message::Binary(encode_relay_message_frame(&frame).into());
                    writing = sink.take().map(|sink| Box::pin(write_owned(sink, message, false)));
                }
                incoming = reader.next() => {
                    // The deadline may have elapsed while select was waiting, even
                    // if the socket branch wins over the timer in this poll.
                    if pong_watchdog.deadline().is_some_and(|deadline| deadline <= tokio::time::Instant::now()) {
                        frames_drained_after_pong_deadline += 1;
                    }
                    match incoming {
                        Some(Ok(Message::Binary(payload))) => {
                            let frame = match decode_relay_message_frame(&payload) {
                                Ok(frame) => frame,
                                Err(error) => { send_malformed(&incoming_tx, error.to_string()); break; }
                            };
                            if frame.stream_id != stream_id { continue; }
                            match frame.validate() {
                                Ok(RelayFrameBodyKind::Data) => {
                                    let result = frame.into_data().and_then(|data| {
                                        for ciphertext in inbound_ciphertexts.push(data.seq, data.payload)? {
                                            let plaintext = transport.decrypt(&ciphertext).map_err(|error|
                                                ExecServerError::Protocol(format!("Noise relay decryption failed: {error}")))?;
                                            for message in inbound_decoder.push(&plaintext)? {
                                                if delivery.len() >= CHANNEL_CAPACITY {
                                                    return Err(ExecServerError::Protocol("Noise relay application staging queue is full".into()));
                                                }
                                                delivery.push_back(JsonRpcConnectionEvent::Message(message));
                                            }
                                        }
                                        Ok(())
                                    });
                                    if let Err(error) = result { send_malformed(&incoming_tx, error.to_string()); break 'relay; }
                                    if !delivery.is_empty() { delivery_deadline.get_or_insert_with(|| tokio::time::Instant::now() + WEBSOCKET_PONG_TIMEOUT); }
                                }
                                Ok(RelayFrameBodyKind::Reset) => {
                                    send_disconnected(&incoming_tx, &disconnected_tx, NOISE_RELAY_RESET_DISCONNECT_REASON.into());
                                    return;
                                }
                                Ok(RelayFrameBodyKind::Ack | RelayFrameBodyKind::Resume | RelayFrameBodyKind::Heartbeat) => {}
                                _ => { send_malformed(&incoming_tx, "Noise relay received invalid post-handshake frame".into()); break; }
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {
                            if ping_in_flight { pong_during_ping = true; }
                            pong_watchdog.received_pong();
                            frames_drained_after_pong_deadline = 0;
                        }
                        Some(Ok(Message::Ping(_) | Message::Frame(_))) => {}
                        Some(Ok(Message::Text(_))) => { send_malformed(&incoming_tx, "Noise relay transport expects binary protobuf frames".into()); break; }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(error)) => { debug!("Noise relay websocket read failed: {error}"); break; }
                    }
                }
            }
        }
        let _ = disconnected_tx.send(true);
    }
    .instrument(stream_span));

    JsonRpcConnection {
        outgoing_tx,
        incoming_rx,
        disconnected_rx,
        task_handles: vec![websocket_task],
        transport: JsonRpcTransport::Plain,
    }
}

async fn write_owned<T, E>(
    mut sink: T,
    message: Message,
    ping: bool,
) -> (T, Result<(), String>, bool)
where
    T: Sink<Message, Error = E> + Unpin,
    E: std::fmt::Display,
{
    let result = send_websocket_message(
        &mut sink,
        message,
        tokio::time::Instant::now() + WEBSOCKET_PONG_TIMEOUT,
    )
    .await;
    (sink, result, ping)
}

async fn send_websocket_message<T, E>(
    websocket: &mut T,
    message: Message,
    deadline: tokio::time::Instant,
) -> Result<(), String>
where
    T: Sink<Message, Error = E> + Unpin,
    E: std::fmt::Display,
{
    match tokio::time::timeout_at(deadline, websocket.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("websocket write timed out".to_string()),
    }
}

fn send_malformed(incoming_tx: &mpsc::Sender<JsonRpcConnectionEvent>, reason: String) {
    let _ = incoming_tx.try_send(JsonRpcConnectionEvent::MalformedMessage { reason });
}

fn send_disconnected(
    incoming_tx: &mpsc::Sender<JsonRpcConnectionEvent>,
    disconnected_tx: &watch::Sender<bool>,
    reason: String,
) {
    let _ = disconnected_tx.send(true);
    let _ = incoming_tx.try_send(JsonRpcConnectionEvent::Disconnected {
        reason: Some(reason),
    });
}

#[cfg(test)]
#[path = "harness_tests.rs"]
mod tests;
