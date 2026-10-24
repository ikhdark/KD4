use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context as TaskContext;
use std::task::Poll;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::JSONRPCRequest;
use codex_exec_server_protocol::JSONRPCResponse;
use codex_exec_server_protocol::RequestId;
use futures::Sink;
use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc as futures_mpsc;
use pretty_assertions::assert_eq;
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::connection::JsonRpcConnectionEvent;
use crate::noise_channel::PendingResponderHandshake;
use crate::relay_proto::RelayData;

const ENVIRONMENT_ID: &str = "environment-1";
const EXECUTOR_REGISTRATION_ID: &str = "registration-1";

fn response_message(id: usize) -> JSONRPCMessage {
    JSONRPCMessage::Response(JSONRPCResponse {
        id: RequestId::Integer(id as i64),
        result: serde_json::json!({ "id": id }),
    })
}

#[tokio::test(start_paused = true)]
async fn normal_close_drains_accepted_responses_in_order() -> Result<()> {
    for eof in [false, true] {
        for fill_channel in [false, true] {
            let (mut connection, mut control, _outbound) = connected_controlled_harness().await?;
            let prefix = if fill_channel { CHANNEL_CAPACITY } else { 0 };
            for seq in 0..prefix {
                control.send_rpc(response_message(seq), seq as u32)?;
            }
            if fill_channel {
                timeout(Duration::from_millis(20), async {
                    while connection.incoming_rx.len() != CHANNEL_CAPACITY {
                        tokio::task::yield_now().await;
                    }
                }).await?;
            }
            for seq in prefix..prefix + 4 {
                control.send_rpc(response_message(seq), seq as u32)?;
            }
            if eof {
                control.inbound_tx.close_channel();
            } else {
                control.send_inbound(Message::Close(None))?;
            }
            timeout(Duration::from_millis(20), async {
                let expected_reads = 1 + prefix + 4 + usize::from(!eof);
                while control.inbound_reads() < expected_reads {
                    tokio::task::yield_now().await;
                }
            }).await?;
            if fill_channel {
                assert!(!*connection.disconnected_rx.borrow(), "close must not abandon staged responses");
            }
            for seq in 0..prefix + 4 {
                let event = timeout(Duration::from_millis(20), connection.incoming_rx.recv()).await?;
                assert!(matches!(event, Some(JsonRpcConnectionEvent::Message(message)) if message == response_message(seq)));
            }
            assert!(timeout(Duration::from_millis(20), connection.incoming_rx.recv()).await?.is_none());
            assert!(*connection.disconnected_rx.borrow());
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn normal_close_drain_keeps_original_backpressure_deadline() -> Result<()> {
    let (mut connection, mut control, _outbound) = connected_controlled_harness().await?;
    for seq in 0..CHANNEL_CAPACITY + 1 {
        control.send_rpc(response_message(seq), seq as u32)?;
    }
    while control.inbound_reads() < CHANNEL_CAPACITY + 2
        || connection.incoming_rx.len() != CHANNEL_CAPACITY
    {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(WEBSOCKET_PONG_TIMEOUT / 2).await;
    control.send_inbound(Message::Close(None))?;
    while control.inbound_reads() < CHANNEL_CAPACITY + 3 {
        tokio::task::yield_now().await;
    }
    assert!(!*connection.disconnected_rx.borrow());
    tokio::time::advance(WEBSOCKET_PONG_TIMEOUT / 2 + Duration::from_millis(1)).await;
    timeout(Duration::from_millis(20), connection.disconnected_rx.changed()).await??;
    assert!(*connection.disconnected_rx.borrow());
    assert!(connection.task_handles[0].is_finished());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn initial_handshake_bounds_resume_handshake_flush_and_silent_peer() -> Result<()> {
    for stage in 0..3 {
        let (mut websocket, _control, mut outbound) = ControlledWebSocket::new(if stage == 0 { 0 } else { 2 });
        if stage == 1 {
            websocket.block_flush_after_sends = Some(2);
        }
        let mut connection = start_controlled_harness(websocket, &NoiseChannelIdentity::generate()?)?;
        let started = tokio::time::Instant::now();
        let event = timeout(NOISE_HANDSHAKE_TIMEOUT + Duration::from_millis(20), connection.incoming_rx.recv()).await?;
        assert!(matches!(event, Some(JsonRpcConnectionEvent::Disconnected { reason: Some(reason) }) if reason == NOISE_HANDSHAKE_TIMEOUT_REASON));
        assert_eq!(started.elapsed(), NOISE_HANDSHAKE_TIMEOUT);
        assert!(*connection.disconnected_rx.borrow());
        let mut sent = 0;
        while outbound.try_recv().is_ok() { sent += 1; }
        assert_eq!(sent, if stage == 0 { 0 } else { 2 });
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn initial_handshake_uses_one_deadline_despite_writes_and_irrelevant_frames() -> Result<()> {
    let (websocket, mut control, mut outbound) = ControlledWebSocket::new(0);
    let mut connection = start_controlled_harness(websocket, &NoiseChannelIdentity::generate()?)?;
    control.wait_for_blocked_write(1).await?;
    tokio::time::advance(NOISE_HANDSHAKE_TIMEOUT / 2).await;
    control.grant_writes(1);
    assert!(matches!(outbound.next().await, Some(Message::Binary(_))));
    control.wait_for_blocked_write(2).await?;
    tokio::time::advance(NOISE_HANDSHAKE_TIMEOUT / 4).await;
    control.grant_writes(1);
    assert!(matches!(outbound.next().await, Some(Message::Binary(_))));
    for index in 0..4 {
        let message = if index % 2 == 0 {
            Message::Pong(Vec::new().into())
        } else {
            Message::Binary(encode_relay_message_frame(&RelayMessageFrame::resume("unrelated".into())).into())
        };
        control.send_inbound(message)?;
        while control.inbound_reads() <= index {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(NOISE_HANDSHAKE_TIMEOUT / 16).await;
    }
    let event = timeout(Duration::from_millis(20), connection.incoming_rx.recv()).await?;
    assert!(matches!(event, Some(JsonRpcConnectionEvent::Disconnected { reason: Some(reason) }) if reason == NOISE_HANDSHAKE_TIMEOUT_REASON));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn initial_handshake_accepts_response_just_before_deadline() -> Result<()> {
    let (mut connection, mut control, _outbound) = connected_controlled_harness_after_delay(
        NOISE_HANDSHAKE_TIMEOUT - Duration::from_millis(1),
    ).await?;
    control.send_rpc(response_message(1), 0)?;
    let event = timeout(Duration::from_millis(20), connection.incoming_rx.recv()).await?;
    assert!(matches!(event, Some(JsonRpcConnectionEvent::Message(message)) if message == response_message(1)));
    assert!(!*connection.disconnected_rx.borrow());
    control.send_inbound(Message::Close(None))?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn dropping_rpc_owner_aborts_pending_handshake() -> Result<()> {
    let (websocket, mut control, _outbound) = ControlledWebSocket::new(0);
    let connection = start_controlled_harness(websocket, &NoiseChannelIdentity::generate()?)?;
    let (client, _events) = crate::rpc::RpcClient::new(connection);
    control.wait_for_blocked_write(1).await?;
    drop(client);
    tokio::task::yield_now().await;
    assert!(control.inbound_tx.is_closed());
    assert!(control.write_permit_tx.is_closed());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn rpc_client_receives_final_noise_response_before_close_or_eof() -> Result<()> {
    for eof in [false, true] {
        let (connection, mut control, mut outbound) = connected_controlled_harness().await?;
        let (client, mut events) = crate::rpc::RpcClient::new(connection);
        let params = serde_json::json!({});
        let mut call = Box::pin(client.call::<_, serde_json::Value>("final", &params));
        assert!(futures::poll!(&mut call).is_pending());
        control.grant_writes(1);
        let data = read_outbound_data(&mut outbound).await?;
        let plaintext = control.transport.as_mut().context("peer transport")?.decrypt(&data.payload)?;
        let requests = JsonRpcMessageDecoder::default().push(&plaintext)?;
        let Some(JSONRPCMessage::Request(request)) = requests.into_iter().next() else {
            anyhow::bail!("expected the owning RpcClient request");
        };
        let expected = serde_json::json!({ "completed": true });
        control.send_rpc(JSONRPCMessage::Response(JSONRPCResponse {
            id: request.id,
            result: expected.clone(),
        }), 0)?;
        if eof {
            control.inbound_tx.close_channel();
        } else {
            control.send_inbound(Message::Close(None))?;
        }
        assert_eq!(timeout(Duration::from_millis(20), call).await?.expect("final RPC response"), expected);
        assert!(matches!(timeout(Duration::from_millis(20), events.recv()).await?,
            Some(crate::rpc::RpcClientEvent::Disconnected { .. })));
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn fragmented_writes_yield_to_keepalive_and_queued_pong() -> Result<()> {
    let (connection, mut control, mut outbound_rx) = connected_controlled_harness().await?;

    connection
        .outgoing_tx
        .send(JSONRPCMessage::Request(JSONRPCRequest {
            id: RequestId::Integer(1),
            method: "large".to_string(),
            params: Some(serde_json::json!({
                "payload": "x".repeat(NOISE_RECORD_PLAINTEXT_LEN * 3),
            })),
            trace: None,
        }))
        .await?;

    control.wait_for_blocked_write(/*expected*/ 1).await?;
    tokio::time::advance(WEBSOCKET_KEEPALIVE_INTERVAL + Duration::from_millis(10)).await;
    control.grant_writes(/*count*/ 1);
    let first_data = read_outbound_data(&mut outbound_rx).await?;
    assert_eq!(first_data.seq, 0);

    control.wait_for_blocked_write(/*expected*/ 2).await?;
    control.grant_writes(/*count*/ 1);
    let Message::Ping(ping_payload) = timeout(Duration::from_secs(1), outbound_rx.next())
        .await?
        .context("harness closed before sending keepalive")?
    else {
        anyhow::bail!("expected keepalive between fragmented writes");
    };

    control.wait_for_blocked_write(/*expected*/ 3).await?;
    control.send_inbound(Message::Pong(ping_payload))?;
    tokio::time::advance(WEBSOCKET_KEEPALIVE_INTERVAL + Duration::from_millis(10)).await;
    control.grant_writes(/*count*/ 1);
    let second_data = read_outbound_data(&mut outbound_rx).await?;
    assert_eq!(second_data.seq, 1);

    control.wait_for_blocked_write(/*expected*/ 4).await?;
    control.grant_writes(/*count*/ 1);
    let next_message = timeout(Duration::from_secs(1), outbound_rx.next())
        .await?
        .context("harness closed after receiving queued Pong")?;
    assert!(matches!(next_message, Message::Ping(_)));

    for task in &connection.task_handles {
        task.abort();
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn post_deadline_drain_stops_before_frame_33() -> Result<()> {
    let (mut connection, mut control, mut outbound_rx) = connected_controlled_harness().await?;

    control.wait_for_blocked_write(/*expected*/ 1).await?;
    control.grant_writes(/*count*/ 1);
    let Message::Ping(ping_payload) = timeout(Duration::from_secs(1), outbound_rx.next())
        .await?
        .context("harness closed before sending keepalive")?
    else {
        anyhow::bail!("expected keepalive ping");
    };
    let reads_before_deadline = control.inbound_reads();

    let unrelated_frame =
        encode_relay_message_frame(&RelayMessageFrame::resume("unrelated-stream".to_string()));
    for _ in 0..MAX_FRAMES_DRAINED_AFTER_PONG_DEADLINE {
        control.send_inbound(Message::Binary(unrelated_frame.clone().into()))?;
    }
    control.send_inbound(Message::Pong(ping_payload))?;

    // Keep the current-thread runtime from consuming the queued frames until the
    // Pong deadline and every frame are ready together.
    tokio::time::advance(WEBSOCKET_PONG_TIMEOUT + Duration::from_millis(10)).await;

    let event = timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?;
    let Some(JsonRpcConnectionEvent::Disconnected { reason }) = event else {
        anyhow::bail!("expected Pong timeout, got {event:?}");
    };
    assert_eq!(reason.as_deref(), Some(WEBSOCKET_PONG_TIMEOUT_REASON));
    assert_eq!(
        control.inbound_reads() - reads_before_deadline,
        MAX_FRAMES_DRAINED_AFTER_PONG_DEADLINE
    );
    Ok(())
}

#[tokio::test]
async fn pong_keeps_harness_alive_until_peer_stops_responding() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let harness_connection = tokio::spawn(connect_async(websocket_url));
    let (socket, _peer_addr) = listener.accept().await?;
    let mut executor_websocket = accept_async(socket).await?;
    let (harness_websocket, _response) = harness_connection.await??;

    let executor_identity = NoiseChannelIdentity::generate()?;
    let mut connection = noise_harness_connection_from_websocket(
        harness_websocket,
        NoiseHarnessConnectionArgs {
            connection_label: "test rendezvous".to_string(),
            environment_id: ENVIRONMENT_ID.to_string(),
            executor_registration_id: EXECUTOR_REGISTRATION_ID.to_string(),
            identity: NoiseChannelIdentity::generate()?,
            responder_public_key: executor_identity.public_key(),
            harness_key_authorization: "authorization".to_string(),
        },
    );

    let resume_message = timeout(Duration::from_secs(1), executor_websocket.next())
        .await?
        .context("harness closed before sending resume")??;
    let Message::Binary(resume_payload) = resume_message else {
        anyhow::bail!("expected resume frame, got {resume_message:?}");
    };
    let resume = decode_relay_message_frame(resume_payload.as_ref())?;
    assert_eq!(resume.validate()?, RelayFrameBodyKind::Resume);

    let handshake_message = timeout(Duration::from_secs(1), executor_websocket.next())
        .await?
        .context("harness closed before sending handshake")??;
    let Message::Binary(handshake_payload) = handshake_message else {
        anyhow::bail!("expected handshake frame, got {handshake_message:?}");
    };
    let handshake = decode_relay_message_frame(handshake_payload.as_ref())?;
    assert_eq!(handshake.stream_id, resume.stream_id);
    let stream_id = handshake.stream_id.clone();
    let prologue =
        noise_channel_prologue(ENVIRONMENT_ID, EXECUTOR_REGISTRATION_ID, stream_id.as_str());
    let pending = PendingResponderHandshake::read_request(
        &executor_identity,
        &prologue,
        &handshake.into_handshake_payload()?,
    )?;
    let (_transport, response) = pending.complete()?;
    let response = RelayMessageFrame::handshake(stream_id, response);
    executor_websocket
        .send(Message::Binary(
            encode_relay_message_frame(&response).into(),
        ))
        .await?;

    let mut pings = 0;
    while pings < 6 {
        let message = timeout(Duration::from_secs(1), executor_websocket.next())
            .await?
            .context("harness disconnected before six keepalive pings")??;
        match message {
            Message::Ping(payload) => {
                executor_websocket.send(Message::Pong(payload)).await?;
                pings += 1;
            }
            Message::Pong(_) | Message::Frame(_) => {}
            message => anyhow::bail!("expected keepalive ping, got {message:?}"),
        }
    }

    // Keep non-Pong traffic flowing after responses stop. It must not defeat
    // the bounded grace for a Pong already queued behind data.
    let unrelated_frame =
        encode_relay_message_frame(&RelayMessageFrame::resume("unrelated-stream".to_string()));
    let traffic_task = tokio::spawn(async move {
        loop {
            if executor_websocket
                .send(Message::Binary(unrelated_frame.clone().into()))
                .await
                .is_err()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let event = timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?;
    traffic_task.abort();
    let _ = traffic_task.await;
    let Some(JsonRpcConnectionEvent::Disconnected { reason }) = event else {
        anyhow::bail!("expected pong timeout, got {event:?}");
    };
    assert_eq!(reason.as_deref(), Some(WEBSOCKET_PONG_TIMEOUT_REASON));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn write_flush_has_an_independent_deadline() -> Result<()> {
    let (mut websocket, _control, mut outbound_rx) = ControlledWebSocket::new(1);
    websocket.block_flush = true;
    let (_, result, ping) = timeout(
        WEBSOCKET_PONG_TIMEOUT + Duration::from_secs(1),
        write_owned(websocket, Message::Ping(Vec::new().into()), true),
    )
    .await?;
    assert_eq!(result, Err("websocket write timed out".to_string()));
    assert!(ping);
    assert!(matches!(outbound_rx.try_recv()?, Message::Ping(_)));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn blocked_write_does_not_block_inbound_control() -> Result<()> {
    let (mut connection, mut control, _outbound) = connected_controlled_harness().await?;
    connection
        .outgoing_tx
        .send(JSONRPCMessage::Request(JSONRPCRequest {
            id: RequestId::Integer(1),
            method: "blocked".into(),
            params: None,
            trace: None,
        }))
        .await?;
    control.wait_for_blocked_write(1).await?;
    let inbound = JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(2),
        method: "inbound".into(),
        params: None,
        trace: None,
    });
    control.send_rpc(inbound.clone(), 0)?;
    let event = timeout(Duration::from_millis(20), connection.incoming_rx.recv()).await?;
    assert!(matches!(event, Some(JsonRpcConnectionEvent::Message(message)) if message == inbound));
    let reads = control.inbound_reads();
    control.send_inbound(Message::Pong(Vec::new().into()))?;
    timeout(Duration::from_millis(20), async {
        while control.inbound_reads() == reads {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("reader must progress before a write permit is granted")?;
    control.send_inbound(Message::Close(None))?;
    timeout(
        Duration::from_millis(20),
        connection.disconnected_rx.changed(),
    )
    .await??;
    assert!(*connection.disconnected_rx.borrow());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn stalled_application_allows_control_reads_then_disconnects() -> Result<()> {
    let (mut connection, mut control, _outbound) = connected_controlled_harness().await?;
    for seq in 0..=CHANNEL_CAPACITY {
        control.send_rpc(
            JSONRPCMessage::Request(JSONRPCRequest {
                id: RequestId::Integer(seq as i64),
                method: "queued".into(),
                params: None,
                trace: None,
            }),
            seq as u32,
        )?;
        while control.inbound_reads() < seq + 2 {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
    }
    let reads = control.inbound_reads();
    control.send_inbound(Message::Pong(Vec::new().into()))?;
    timeout(Duration::from_millis(20), async {
        while control.inbound_reads() == reads {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(!*connection.disconnected_rx.borrow());
    tokio::time::advance(WEBSOCKET_PONG_TIMEOUT + Duration::from_millis(1)).await;
    timeout(
        Duration::from_millis(20),
        connection.disconnected_rx.changed(),
    )
    .await??;
    assert!(*connection.disconnected_rx.borrow());
    Ok(())
}

async fn read_outbound_data(
    outbound_rx: &mut futures_mpsc::UnboundedReceiver<Message>,
) -> Result<RelayData> {
    let Message::Binary(payload) = timeout(Duration::from_secs(1), outbound_rx.next())
        .await?
        .context("harness closed before sending data")?
    else {
        anyhow::bail!("expected relay data frame");
    };
    let frame = decode_relay_message_frame(payload.as_ref())?;
    assert_eq!(frame.validate()?, RelayFrameBodyKind::Data);
    frame.into_data().map_err(anyhow::Error::from)
}

async fn connected_controlled_harness() -> Result<(
    JsonRpcConnection,
    ControlledWebSocketHandle,
    futures_mpsc::UnboundedReceiver<Message>,
)> {
    connected_controlled_harness_after_delay(Duration::ZERO).await
}

fn start_controlled_harness(
    websocket: ControlledWebSocket,
    executor_identity: &NoiseChannelIdentity,
) -> Result<JsonRpcConnection> {
    Ok(noise_harness_connection_from_websocket(
        websocket,
        NoiseHarnessConnectionArgs {
            connection_label: "test rendezvous".to_string(),
            environment_id: ENVIRONMENT_ID.to_string(),
            executor_registration_id: EXECUTOR_REGISTRATION_ID.to_string(),
            identity: NoiseChannelIdentity::generate()?,
            responder_public_key: executor_identity.public_key(),
            harness_key_authorization: "authorization".to_string(),
        },
    ))
}

async fn connected_controlled_harness_after_delay(delay: Duration) -> Result<(
    JsonRpcConnection,
    ControlledWebSocketHandle,
    futures_mpsc::UnboundedReceiver<Message>,
)> {
    let (websocket, mut control, mut outbound_rx) =
        ControlledWebSocket::new(/*write_permits*/ 2);
    let executor_identity = NoiseChannelIdentity::generate()?;
    let connection = start_controlled_harness(websocket, &executor_identity)?;

    let Message::Binary(resume_payload) = timeout(Duration::from_secs(1), outbound_rx.next())
        .await?
        .context("harness closed before sending resume")?
    else {
        anyhow::bail!("expected resume frame");
    };
    let resume = decode_relay_message_frame(resume_payload.as_ref())?;
    let Message::Binary(handshake_payload) = timeout(Duration::from_secs(1), outbound_rx.next())
        .await?
        .context("harness closed before sending handshake")?
    else {
        anyhow::bail!("expected handshake frame");
    };
    let handshake = decode_relay_message_frame(handshake_payload.as_ref())?;
    let stream_id = handshake.stream_id.clone();
    control.stream_id = stream_id.clone();
    assert_eq!(stream_id, resume.stream_id);
    let prologue =
        noise_channel_prologue(ENVIRONMENT_ID, EXECUTOR_REGISTRATION_ID, stream_id.as_str());
    let pending = PendingResponderHandshake::read_request(
        &executor_identity,
        &prologue,
        &handshake.into_handshake_payload()?,
    )?;
    let (transport, response) = pending.complete()?;
    control.transport = Some(transport);
    tokio::time::sleep(delay).await;
    control.send_inbound(Message::Binary(
        encode_relay_message_frame(&RelayMessageFrame::handshake(stream_id, response)).into(),
    ))?;
    Ok((connection, control, outbound_rx))
}

struct ControlledWebSocket {
    inbound_rx: futures_mpsc::UnboundedReceiver<Result<Message, std::convert::Infallible>>,
    outbound_tx: futures_mpsc::UnboundedSender<Message>,
    write_permit_rx: futures_mpsc::UnboundedReceiver<()>,
    blocked_write_tx: futures_mpsc::UnboundedSender<usize>,
    write_waiting: bool,
    blocked_writes: usize,
    inbound_reads: Arc<AtomicUsize>,
    block_flush: bool,
    block_flush_after_sends: Option<usize>,
    sent_messages: usize,
}

struct ControlledWebSocketHandle {
    stream_id: String,
    transport: Option<crate::noise_channel::NoiseTransport>,
    inbound_tx: futures_mpsc::UnboundedSender<Result<Message, std::convert::Infallible>>,
    write_permit_tx: futures_mpsc::UnboundedSender<()>,
    blocked_write_rx: futures_mpsc::UnboundedReceiver<usize>,
    inbound_reads: Arc<AtomicUsize>,
}

impl ControlledWebSocket {
    fn new(
        write_permits: usize,
    ) -> (
        Self,
        ControlledWebSocketHandle,
        futures_mpsc::UnboundedReceiver<Message>,
    ) {
        let (inbound_tx, inbound_rx) = futures_mpsc::unbounded();
        let (outbound_tx, outbound_rx) = futures_mpsc::unbounded();
        let (write_permit_tx, write_permit_rx) = futures_mpsc::unbounded();
        let (blocked_write_tx, blocked_write_rx) = futures_mpsc::unbounded();
        for _ in 0..write_permits {
            write_permit_tx
                .unbounded_send(())
                .expect("test write permit receiver should stay open");
        }
        let inbound_reads = Arc::new(AtomicUsize::new(0));
        (
            Self {
                inbound_rx,
                outbound_tx,
                write_permit_rx,
                blocked_write_tx,
                write_waiting: false,
                blocked_writes: 0,
                inbound_reads: Arc::clone(&inbound_reads),
                block_flush: false,
                block_flush_after_sends: None,
                sent_messages: 0,
            },
            ControlledWebSocketHandle {
                stream_id: String::new(),
                transport: None,
                inbound_tx,
                write_permit_tx,
                blocked_write_rx,
                inbound_reads,
            },
            outbound_rx,
        )
    }
}

impl ControlledWebSocketHandle {
    fn send_rpc(&mut self, message: JSONRPCMessage, seq: u32) -> Result<()> {
        let bytes = frame_jsonrpc_message(&message)?;
        let encrypted = self
            .transport
            .as_mut()
            .context("handshake not complete")?
            .encrypt(&bytes)?;
        self.send_inbound(Message::Binary(
            encode_relay_message_frame(&RelayMessageFrame::data(
                self.stream_id.clone(),
                seq,
                encrypted,
            ))
            .into(),
        ))
    }

    fn send_inbound(&self, message: Message) -> Result<()> {
        self.inbound_tx
            .unbounded_send(Ok(message))
            .map_err(anyhow::Error::from)
    }

    fn grant_writes(&self, count: usize) {
        for _ in 0..count {
            self.write_permit_tx
                .unbounded_send(())
                .expect("test write permit receiver should stay open");
        }
    }

    fn inbound_reads(&self) -> usize {
        self.inbound_reads.load(Ordering::Acquire)
    }

    async fn wait_for_blocked_write(&mut self, expected: usize) -> Result<()> {
        let actual = timeout(Duration::from_secs(1), self.blocked_write_rx.next())
            .await?
            .context("websocket closed before blocking the expected write")?;
        assert_eq!(actual, expected);
        Ok(())
    }
}

impl Sink<Message> for ControlledWebSocket {
    type Error = std::convert::Infallible;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        match Pin::new(&mut this.write_permit_rx).poll_next(cx) {
            Poll::Ready(Some(())) => {
                this.write_waiting = false;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) | Poll::Pending => {
                if !this.write_waiting {
                    this.write_waiting = true;
                    this.blocked_writes += 1;
                    this.blocked_write_tx
                        .unbounded_send(this.blocked_writes)
                        .expect("test blocked-write receiver should stay open");
                }
                Poll::Pending
            }
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        self.sent_messages += 1;
        self.outbound_tx
            .unbounded_send(item)
            .expect("test outbound receiver should stay open");
        Ok(())
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        if self.block_flush
            || self.block_flush_after_sends.is_some_and(|count| self.sent_messages >= count)
        {
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl futures::Stream for ControlledWebSocket {
    type Item = Result<Message, std::convert::Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let result = Pin::new(&mut self.inbound_rx).poll_next(cx);
        if matches!(result, Poll::Ready(Some(_))) {
            self.inbound_reads.fetch_add(1, Ordering::Release);
        }
        result
    }
}

#[tokio::test(start_paused = true)]
async fn harness_expires_gap_without_needing_another_data_frame() -> Result<()> {
    let (mut connection, control, _outbound) = connected_controlled_harness().await?;
    control.grant_writes(16);
    control.send_inbound(Message::Binary(
        encode_relay_message_frame(&RelayMessageFrame::data(
            control.stream_id.clone(),
            1,
            vec![1],
        ))
        .into(),
    ))?;
    // Two reads are the handshake response and the future ciphertext.
    while control.inbound_reads() < 2 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(
        crate::noise_relay::ordered_ciphertext::CIPHERTEXT_GAP_TIMEOUT + Duration::from_secs(1),
    )
    .await;
    let event = timeout(Duration::from_secs(1), connection.incoming_rx.recv()).await?;
    assert!(
        matches!(event, Some(JsonRpcConnectionEvent::Disconnected { reason: Some(reason) })
        if reason == crate::noise_relay::ordered_ciphertext::CIPHERTEXT_GAP_TIMEOUT_REASON)
    );
    Ok(())
}
