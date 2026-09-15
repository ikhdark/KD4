use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use super::HarnessKeyValidator;
use super::MAX_FAILED_NOISE_HANDSHAKES;
use super::MAX_HARNESS_KEY_AUTHORIZATION_BYTES;
use super::RendezvousDisconnectReason;
use super::run_multiplexed_environment;
use crate::ExecServerError;
use crate::ExecServerRuntimePaths;
use crate::noise_channel::InitiatorHandshake;
use crate::noise_channel::NoiseChannelIdentity;
use crate::noise_channel::NoiseChannelPublicKey;
use crate::noise_channel::noise_channel_prologue;
use crate::relay::RelayFrameBodyKind;
use crate::relay::decode_relay_message_frame;
use crate::relay::encode_relay_message_frame;
use crate::relay_proto::RelayMessageFrame;
use crate::server::ConnectionProcessor;

const ENVIRONMENT_ID: &str = "environment-1";
const EXECUTOR_REGISTRATION_ID: &str = "registration-1";

async fn read_reset<S>(
    websocket: &mut tokio_tungstenite::WebSocketStream<S>,
) -> Result<RelayMessageFrame>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    timeout(Duration::from_secs(1), async {
        loop {
            match websocket.next().await {
                Some(Ok(Message::Binary(payload))) => {
                    let frame = decode_relay_message_frame(&payload)?;
                    assert_eq!(frame.validate()?, RelayFrameBodyKind::Reset);
                    return Ok(frame);
                }
                Some(Ok(Message::Ping(payload))) => websocket.send(Message::Pong(payload)).await?,
                Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                other => anyhow::bail!("expected reset, got {other:?}"),
            }
        }
    })
    .await?
}

async fn expect_budget_exhausted<S>(
    mut task: tokio::task::JoinHandle<RendezvousDisconnectReason>,
    websocket: &mut tokio_tungstenite::WebSocketStream<S>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let reason = timeout(Duration::from_secs(1), async {
        loop {
            tokio::select! {
                result = &mut task => return result,
                message = websocket.next() => match message {
                    Some(Ok(Message::Ping(payload))) => { let _ = websocket.send(Message::Pong(payload)).await; }
                    Some(Ok(_)) => {}
                    _ => return task.await,
                }
            }
        }
    }).await??;
    assert_eq!(reason, RendezvousDisconnectReason::HandshakeBudgetExhausted);
    Ok(())
}

#[tokio::test]
async fn missing_pong_disconnects_physical_relay() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let harness_connection = tokio::spawn(connect_async(websocket_url));
    let (socket, _peer_addr) = listener.accept().await?;
    let environment_websocket = accept_async(socket).await?;
    let (_harness_websocket, _response) = harness_connection.await??;

    let environment_task = tokio::spawn(run_multiplexed_environment(
        environment_websocket,
        ConnectionProcessor::new(ExecServerRuntimePaths::new(std::env::current_exe()?)?),
        ENVIRONMENT_ID.to_string(),
        EXECUTOR_REGISTRATION_ID.to_string(),
        NoiseChannelIdentity::generate()?,
        BlockingValidator {
            calls: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(Notify::new()),
        },
    ));

    assert_eq!(
        timeout(Duration::from_secs(1), environment_task).await??,
        RendezvousDisconnectReason::PongTimeout
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn full_handshake_reply_queue_preserves_existing_stream() -> Result<()> {
    use crate::relay_proto::relay_message_frame::Body;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::protocol::Role;

    // A small duplex buffer makes outgoing backpressure deterministic.
    let (harness_io, environment_io) = tokio::io::duplex(64);
    let mut harness = WebSocketStream::from_raw_socket(harness_io, Role::Client, None).await;
    let environment = WebSocketStream::from_raw_socket(environment_io, Role::Server, None).await;
    let identity = NoiseChannelIdentity::generate()?;
    let harness_identity = NoiseChannelIdentity::generate()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let mut task = tokio::spawn(run_multiplexed_environment(
        environment,
        ConnectionProcessor::new(ExecServerRuntimePaths::new(std::env::current_exe()?)?),
        ENVIRONMENT_ID.into(),
        EXECUTOR_REGISTRATION_ID.into(),
        identity.clone(),
        BlockingValidator {
            calls: calls.clone(),
            release: release.clone(),
        },
    ));
    let (handshake, request) = InitiatorHandshake::start(
        &harness_identity,
        &identity.public_key(),
        &noise_channel_prologue(ENVIRONMENT_ID, EXECUTOR_REGISTRATION_ID, "existing"),
        b"authorization",
    )?;
    harness
        .send(Message::Binary(
            encode_relay_message_frame(&RelayMessageFrame::handshake("existing".into(), request))
                .into(),
        ))
        .await?;
    timeout(Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    release.notify_one();
    let Message::Binary(response) = harness.next().await.expect("handshake response")? else {
        anyhow::bail!("expected binary handshake reply");
    };
    let Some(Body::Handshake(response)) = decode_relay_message_frame(&response)?.body else {
        anyhow::bail!("expected handshake reply");
    };
    let mut transport = handshake.finish(&response.payload)?;

    // Unknown stream data queues resets without charging the handshake budget.
    for i in 0..crate::connection::CHANNEL_CAPACITY * 2 {
        harness
            .send(Message::Binary(
                encode_relay_message_frame(&RelayMessageFrame::data(
                    format!("unknown-{i}"),
                    0,
                    vec![0],
                ))
                .into(),
            ))
            .await?;
    }
    let (_, request) = InitiatorHandshake::start(
        &harness_identity,
        &identity.public_key(),
        &noise_channel_prologue(ENVIRONMENT_ID, EXECUTOR_REGISTRATION_ID, "new"),
        b"authorization",
    )?;
    harness
        .send(Message::Binary(
            encode_relay_message_frame(&RelayMessageFrame::handshake("new".into(), request)).into(),
        ))
        .await?;
    timeout(Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    release.notify_one();
    // Give validation completion a turn while keeping the writer blocked.
    // This is shorter than the physical write deadline (100 ms in tests).
    assert!(timeout(Duration::from_millis(50), &mut task).await.is_err());

    let request = serde_json::to_vec(&serde_json::json!({
        "id": 7, "method": "queue-probe"
    }))?;
    let mut framed = (request.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&request);
    let payload = transport.encrypt(&framed)?;
    harness
        .send(Message::Binary(
            encode_relay_message_frame(&RelayMessageFrame::data("existing".into(), 0, payload))
                .into(),
        ))
        .await?;
    let response = timeout(Duration::from_secs(1), async {
        loop {
            match harness.next().await {
                Some(Ok(Message::Binary(payload))) => {
                    let frame = decode_relay_message_frame(&payload)?;
                    if frame.stream_id == "existing" {
                        let Some(Body::Data(data)) = frame.body else {
                            anyhow::bail!("existing stream reset");
                        };
                        let plaintext = transport.decrypt(&data.payload)?;
                        assert!(plaintext.len() >= 4);
                        let length = u32::from_be_bytes(plaintext[..4].try_into()?) as usize;
                        assert_eq!(length, plaintext.len() - 4);
                        return Ok::<_, anyhow::Error>(
                            serde_json::from_slice::<serde_json::Value>(&plaintext[4..])?,
                        );
                    }
                }
                Some(Ok(Message::Ping(payload))) => harness.send(Message::Pong(payload)).await?,
                other => anyhow::bail!("expected existing stream response, got {other:?}"),
            }
        }
    })
    .await??;
    assert_eq!(response["id"], 7);
    assert_eq!(response["error"]["code"], -32601);
    assert_eq!(
        response["error"]["message"],
        "exec-server stub does not implement `queue-probe` yet"
    );
    task.abort();
    let _ = task.await;
    Ok(())
}

#[tokio::test]
async fn pong_keeps_physical_relay_connected() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let harness_connection = tokio::spawn(connect_async(websocket_url));
    let (socket, _peer_addr) = listener.accept().await?;
    let environment_websocket = accept_async(socket).await?;
    let (mut harness_websocket, _response) = harness_connection.await??;

    let environment_task = tokio::spawn(run_multiplexed_environment(
        environment_websocket,
        ConnectionProcessor::new(ExecServerRuntimePaths::new(std::env::current_exe()?)?),
        ENVIRONMENT_ID.to_string(),
        EXECUTOR_REGISTRATION_ID.to_string(),
        NoiseChannelIdentity::generate()?,
        BlockingValidator {
            calls: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(Notify::new()),
        },
    ));

    timeout(Duration::from_secs(1), async {
        let mut pings = 0;
        while pings < 6 {
            match harness_websocket.next().await {
                Some(Ok(Message::Ping(payload))) => {
                    harness_websocket.send(Message::Pong(payload)).await?;
                    pings += 1;
                }
                Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(message)) => anyhow::bail!("expected keepalive ping, got {message:?}"),
                Some(Err(error)) => return Err(error.into()),
                None => anyhow::bail!("environment disconnected before six keepalive pings"),
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    harness_websocket.close(None).await?;
    assert_eq!(
        timeout(Duration::from_secs(1), environment_task).await??,
        RendezvousDisconnectReason::PeerClose
    );
    Ok(())
}

#[derive(Clone)]
struct BlockingValidator {
    calls: Arc<AtomicUsize>,
    release: Arc<Notify>,
}

impl HarnessKeyValidator for BlockingValidator {
    fn validate_harness_key(
        &self,
        _harness_public_key: &NoiseChannelPublicKey,
        _authorization: &str,
    ) -> impl std::future::Future<Output = Result<(), ExecServerError>> + Send {
        let calls = Arc::clone(&self.calls);
        let release = Arc::clone(&self.release);
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            release.notified().await;
            Ok(())
        }
    }
}

#[tokio::test]
async fn pending_harness_key_validation_does_not_block_new_handshakes() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let harness_connection = tokio::spawn(connect_async(websocket_url));
    let (socket, _peer_addr) = listener.accept().await?;
    let environment_websocket = accept_async(socket).await?;
    let (mut harness_websocket, _response) = harness_connection.await??;

    let environment_identity = NoiseChannelIdentity::generate()?;
    let harness_identity = NoiseChannelIdentity::generate()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let environment_task = tokio::spawn(run_multiplexed_environment(
        environment_websocket,
        ConnectionProcessor::new(ExecServerRuntimePaths::new(std::env::current_exe()?)?),
        ENVIRONMENT_ID.to_string(),
        EXECUTOR_REGISTRATION_ID.to_string(),
        environment_identity.clone(),
        BlockingValidator {
            calls: Arc::clone(&calls),
            release: Arc::new(Notify::new()),
        },
    ));

    for stream_id in ["stream-1", "stream-2"] {
        let prologue = noise_channel_prologue(ENVIRONMENT_ID, EXECUTOR_REGISTRATION_ID, stream_id);
        let (_handshake, request) = InitiatorHandshake::start(
            &harness_identity,
            &environment_identity.public_key(),
            &prologue,
            b"authorization",
        )?;
        let frame = RelayMessageFrame::handshake(stream_id.to_string(), request);
        harness_websocket
            .send(Message::Binary(encode_relay_message_frame(&frame).into()))
            .await?;
    }

    timeout(Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await?;

    harness_websocket.close(None).await?;
    timeout(Duration::from_secs(1), environment_task).await??;
    Ok(())
}

#[tokio::test]
async fn duplicate_handshakes_exhaust_failure_budget() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let harness_connection = tokio::spawn(connect_async(websocket_url));
    let (socket, _peer_addr) = listener.accept().await?;
    let environment_websocket = accept_async(socket).await?;
    let (mut harness_websocket, _response) = harness_connection.await??;

    let environment_identity = NoiseChannelIdentity::generate()?;
    let harness_identity = NoiseChannelIdentity::generate()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let environment_task = tokio::spawn(run_multiplexed_environment(
        environment_websocket,
        ConnectionProcessor::new(ExecServerRuntimePaths::new(std::env::current_exe()?)?),
        ENVIRONMENT_ID.to_string(),
        EXECUTOR_REGISTRATION_ID.to_string(),
        environment_identity.clone(),
        BlockingValidator {
            calls: Arc::clone(&calls),
            release: Arc::clone(&release),
        },
    ));

    let stream_id = "stream-1";
    let prologue = noise_channel_prologue(ENVIRONMENT_ID, EXECUTOR_REGISTRATION_ID, stream_id);
    let (_handshake, request) = InitiatorHandshake::start(
        &harness_identity,
        &environment_identity.public_key(),
        &prologue,
        b"authorization",
    )?;
    let frame = RelayMessageFrame::handshake(stream_id.to_string(), request);
    let encoded = encode_relay_message_frame(&frame);
    harness_websocket
        .send(Message::Binary(encoded.clone().into()))
        .await?;
    timeout(Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await?;

    for attempt in 1..MAX_FAILED_NOISE_HANDSHAKES {
        if attempt > 1 {
            harness_websocket
                .send(Message::Binary(encoded.clone().into()))
                .await?;
            timeout(Duration::from_secs(1), async {
                while calls.load(Ordering::SeqCst) != attempt {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
        }
        harness_websocket
            .send(Message::Binary(encoded.clone().into()))
            .await?;
        let reset = read_reset(&mut harness_websocket).await?;
        assert_eq!(reset.stream_id, stream_id);
        assert_eq!(reset.validate()?, RelayFrameBodyKind::Reset);
    }

    harness_websocket
        .send(Message::Binary(encoded.clone().into()))
        .await?;
    timeout(Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) != MAX_FAILED_NOISE_HANDSHAKES {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    harness_websocket
        .send(Message::Binary(encoded.into()))
        .await?;
    expect_budget_exhausted(environment_task, &mut harness_websocket).await?;
    release.notify_waiters();
    Ok(())
}

#[tokio::test]
async fn oversized_harness_authorization_is_rejected_before_validation() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let harness_connection = tokio::spawn(connect_async(websocket_url));
    let (socket, _peer_addr) = listener.accept().await?;
    let environment_websocket = accept_async(socket).await?;
    let (mut harness_websocket, _response) = harness_connection.await??;

    let environment_identity = NoiseChannelIdentity::generate()?;
    let harness_identity = NoiseChannelIdentity::generate()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let environment_task = tokio::spawn(run_multiplexed_environment(
        environment_websocket,
        ConnectionProcessor::new(ExecServerRuntimePaths::new(std::env::current_exe()?)?),
        ENVIRONMENT_ID.to_string(),
        EXECUTOR_REGISTRATION_ID.to_string(),
        environment_identity.clone(),
        BlockingValidator {
            calls: Arc::clone(&calls),
            release: Arc::new(Notify::new()),
        },
    ));

    let stream_id = "stream-1";
    let prologue = noise_channel_prologue(ENVIRONMENT_ID, EXECUTOR_REGISTRATION_ID, stream_id);
    let oversized_authorization = vec![b'a'; MAX_HARNESS_KEY_AUTHORIZATION_BYTES + 1];
    let (_handshake, request) = InitiatorHandshake::start(
        &harness_identity,
        &environment_identity.public_key(),
        &prologue,
        &oversized_authorization,
    )?;
    let frame = RelayMessageFrame::handshake(stream_id.to_string(), request);
    harness_websocket
        .send(Message::Binary(encode_relay_message_frame(&frame).into()))
        .await?;

    let reset = read_reset(&mut harness_websocket).await?;
    assert_eq!(reset.validate()?, RelayFrameBodyKind::Reset);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    harness_websocket.close(None).await?;
    timeout(Duration::from_secs(1), environment_task).await??;
    Ok(())
}

#[tokio::test]
async fn repeated_malformed_handshakes_close_the_physical_relay() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let harness_connection = tokio::spawn(connect_async(websocket_url));
    let (socket, _peer_addr) = listener.accept().await?;
    let environment_websocket = accept_async(socket).await?;
    let (mut harness_websocket, _response) = harness_connection.await??;

    let environment_identity = NoiseChannelIdentity::generate()?;
    let harness_identity = NoiseChannelIdentity::generate()?;
    let environment_task = tokio::spawn(run_multiplexed_environment(
        environment_websocket,
        ConnectionProcessor::new(ExecServerRuntimePaths::new(std::env::current_exe()?)?),
        ENVIRONMENT_ID.to_string(),
        EXECUTOR_REGISTRATION_ID.to_string(),
        environment_identity.clone(),
        BlockingValidator {
            calls: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(Notify::new()),
        },
    ));

    for attempt in 0..MAX_FAILED_NOISE_HANDSHAKES {
        let stream_id = format!("malformed-{attempt}");
        let prologue = noise_channel_prologue(ENVIRONMENT_ID, EXECUTOR_REGISTRATION_ID, &stream_id);
        let (_handshake, mut request) = InitiatorHandshake::start(
            &harness_identity,
            &environment_identity.public_key(),
            &prologue,
            b"authorization",
        )?;
        let last_byte = request.last_mut().expect("handshake request is not empty");
        *last_byte ^= 1;
        let frame = RelayMessageFrame::handshake(stream_id.clone(), request);
        harness_websocket
            .send(Message::Binary(encode_relay_message_frame(&frame).into()))
            .await?;
        if attempt + 1 < MAX_FAILED_NOISE_HANDSHAKES {
            assert_eq!(
                read_reset(&mut harness_websocket).await?.stream_id,
                stream_id
            );
            assert!(!environment_task.is_finished());
        }
    }

    expect_budget_exhausted(environment_task, &mut harness_websocket).await?;
    Ok(())
}

#[test_case::test_case(false; "early_data")]
#[test_case::test_case(true; "reset")]
#[tokio::test]
async fn repeated_cancellation_during_validation_exhausts_budget(reset: bool) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let harness_connection = tokio::spawn(connect_async(websocket_url));
    let (socket, _peer_addr) = listener.accept().await?;
    let environment_websocket = accept_async(socket).await?;
    let (mut harness_websocket, _response) = harness_connection.await??;

    let environment_identity = NoiseChannelIdentity::generate()?;
    let harness_identity = NoiseChannelIdentity::generate()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let environment_task = tokio::spawn(run_multiplexed_environment(
        environment_websocket,
        ConnectionProcessor::new(ExecServerRuntimePaths::new(std::env::current_exe()?)?),
        ENVIRONMENT_ID.to_string(),
        EXECUTOR_REGISTRATION_ID.to_string(),
        environment_identity.clone(),
        BlockingValidator {
            calls: Arc::clone(&calls),
            release: Arc::new(Notify::new()),
        },
    ));

    for attempt in 0..MAX_FAILED_NOISE_HANDSHAKES {
        let stream_id = format!("early-data-{attempt}");
        let prologue = noise_channel_prologue(ENVIRONMENT_ID, EXECUTOR_REGISTRATION_ID, &stream_id);
        let (_handshake, request) = InitiatorHandshake::start(
            &harness_identity,
            &environment_identity.public_key(),
            &prologue,
            b"authorization",
        )?;
        let frame = RelayMessageFrame::handshake(stream_id.clone(), request);
        harness_websocket
            .send(Message::Binary(encode_relay_message_frame(&frame).into()))
            .await?;
        timeout(Duration::from_secs(5), async {
            while calls.load(Ordering::SeqCst) != attempt + 1 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await?;
        let frame = if reset {
            RelayMessageFrame::reset(stream_id.clone(), "cancelled".to_string())
        } else {
            RelayMessageFrame::data(stream_id.clone(), 0, vec![0])
        };
        harness_websocket
            .send(Message::Binary(encode_relay_message_frame(&frame).into()))
            .await?;
        if !reset && attempt + 1 < MAX_FAILED_NOISE_HANDSHAKES {
            assert_eq!(
                read_reset(&mut harness_websocket).await?.stream_id,
                stream_id
            );
        }
    }

    expect_budget_exhausted(environment_task, &mut harness_websocket).await?;
    Ok(())
}
