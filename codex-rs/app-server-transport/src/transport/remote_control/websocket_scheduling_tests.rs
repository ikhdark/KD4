use super::*;
use super::tests::connected_websocket_pair;
use super::tests::server_envelope;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use std::task::{Context, Poll};
use tokio::sync::Notify;
use tokio::time::{Duration, Instant, Sleep};

#[derive(Default)]
struct Observed {
    frames: Vec<(Instant, tungstenite::Message)>,
    flushes: usize,
}

struct RecordingSink {
    observed: Arc<StdMutex<Observed>>,
    wake: Arc<Notify>,
    pace: Duration,
    delay: Option<Pin<Box<Sleep>>>,
}

impl futures::Sink<tungstenite::Message> for RecordingSink {
    type Error = tungstenite::Error;
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if let Some(delay) = self.delay.as_mut() {
            if std::future::Future::poll(delay.as_mut(), cx).is_pending() { return Poll::Pending; }
        }
        self.delay = None;
        Poll::Ready(Ok(()))
    }
    fn start_send(mut self: Pin<&mut Self>, frame: tungstenite::Message) -> Result<(), Self::Error> {
        self.observed.lock().unwrap().frames.push((Instant::now(), frame));
        if !self.pace.is_zero() { self.delay = Some(Box::pin(tokio::time::sleep(self.pace))); }
        self.wake.notify_one();
        Ok(())
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.observed.lock().unwrap().flushes += 1;
        Poll::Ready(Ok(()))
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.poll_flush(cx)
    }
}

fn writer_state() -> (Arc<Mutex<WebsocketState>>, watch::Receiver<usize>) {
    let (outbound_buffer, used) = BoundedOutboundBuffer::new();
    (Arc::new(Mutex::new(WebsocketState {
        outbound_buffer, subscribe_cursor: None, next_seq_id_by_stream: HashMap::new(),
        last_completed_client_chunk_seq_id_by_stream: HashMap::new(),
        client_segment_reassembler: ClientSegmentReassembler::default(),
    })), used)
}

fn queued(stream: &str, summary: &str) -> super::super::QueuedServerEnvelope {
    let envelope = server_envelope(&ClientId("client".into()), stream, 1, summary);
    super::super::QueuedServerEnvelope {
        event: envelope.event, client_id: envelope.client_id, stream_id: envelope.stream_id,
        write_complete_tx: None,
    }
}

async fn wait_for_texts(observed: &StdMutex<Observed>, wake: &Notify, count: usize) {
    loop {
        let notified = wake.notified();
        if observed.lock().unwrap().frames.iter().filter(|(_, frame)| frame.is_text()).count() >= count { return; }
        notified.await;
    }
}

#[tokio::test]
async fn live_ready_bursts_batch_without_coalescing_delay() {
    for count in [1, 100] {
        let (state, used) = writer_state();
        let (tx, rx) = mpsc::channel(128);
        let mut completions = Vec::new();
        for index in 0..count {
            let mut envelope = queued("stream", &format!("message-{index}"));
            let (complete, completion) = oneshot::channel();
            envelope.write_complete_tx = Some(complete);
            completions.push(completion);
            tx.try_send(envelope).unwrap();
        }
        let observed = Arc::new(StdMutex::new(Observed::default()));
        let wake = Arc::new(Notify::new());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(RemoteControlWebsocket::run_server_writer_inner(
            state.clone(), Arc::new(Mutex::new(rx)), used,
            RecordingSink { observed: observed.clone(), wake: wake.clone(), pace: Duration::ZERO, delay: None },
            Duration::from_secs(60), shutdown.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            for completion in completions { completion.await.unwrap(); }
        }).await.expect("ready burst flushes immediately");
        shutdown.cancel();
        task.await.unwrap().unwrap();
        let observed = observed.lock().unwrap();
        assert_eq!(observed.flushes, (count as usize).div_ceil(64));
        for (index, (_, frame)) in observed.frames.iter().enumerate() {
            let tungstenite::Message::Text(text) = frame else { panic!("unexpected heartbeat") };
            let expected = server_envelope(&ClientId("client".into()), "stream", index as u64 + 1, &format!("message-{index}"));
            assert_eq!(text.as_str(), serde_json::to_string(&expected).unwrap());
        }
        drop(observed);
        assert_eq!(state.lock().await.outbound_buffer.server_envelopes().count(), count);
    }
}

#[tokio::test(start_paused = true)]
async fn heartbeat_and_stream_fairness_cover_replay_and_segmented_live_output() {
    for live_bulk in [false, true] {
        let (state, used) = writer_state();
        let (tx, rx) = mpsc::channel(128);
        let expected_bulk = if live_bulk {
            let envelope = queued("bulk", &"x".repeat(1024 * 1024));
            let count = encode_server_envelope_for_transport(ServerEnvelope {
                event: envelope.event.clone(), client_id: envelope.client_id.clone(),
                stream_id: envelope.stream_id.clone(), seq_id: 1,
            }).unwrap().len();
            tx.try_send(envelope).unwrap();
            count
        } else {
            for seq_id in 1..=20 {
                state.lock().await.outbound_buffer.insert(&server_envelope(&ClientId("client".into()), "bulk", seq_id, "x"));
            }
            20
        };
        let mut interactive = queued("interactive", "tiny");
        let (complete_tx, complete_rx) = oneshot::channel();
        interactive.write_complete_tx = Some(complete_tx);
        tx.try_send(interactive).unwrap();
        let observed = Arc::new(StdMutex::new(Observed::default()));
        let wake = Arc::new(Notify::new());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(RemoteControlWebsocket::run_server_writer_inner(
            state, Arc::new(Mutex::new(rx)), used,
            RecordingSink { observed: observed.clone(), wake: wake.clone(), pace: Duration::from_secs(8), delay: None },
            Duration::from_secs(10), shutdown.clone(),
        ));
        complete_rx.await.expect("independent stream completes during bulk output");
        assert!(observed.lock().unwrap().frames.iter().filter(|(_, frame)| frame.is_text()).count() <= 3);
        wait_for_texts(&observed, &wake, expected_bulk + 1).await;
        shutdown.cancel();
        task.await.unwrap().unwrap();
        let observed = observed.lock().unwrap();
        let pings = observed.frames.iter().filter(|(_, frame)| frame.is_ping()).map(|(at, _)| *at).collect::<Vec<_>>();
        assert!(!pings.is_empty(), "transfer spans more than one Pong timeout");
        assert!(pings.windows(2).all(|pair| pair[1] - pair[0] < Duration::from_secs(60)));
        let texts = observed.frames.iter().filter_map(|(_, frame)| {
            if let tungstenite::Message::Text(text) = frame { Some(serde_json::from_str::<serde_json::Value>(text).unwrap()) } else { None }
        }).collect::<Vec<_>>();
        let bulk = texts.iter().filter(|frame| frame["stream_id"] == "bulk").collect::<Vec<_>>();
        for (index, frame) in bulk.iter().enumerate() {
            if live_bulk { assert_eq!(frame["segment_id"], index); }
            else { assert_eq!(frame["seq_id"], index + 1); }
        }
    }
}

#[test]
fn repaired_retry_is_typed_and_bounded_without_resetting_backoff() {
    let mut available = true;
    let mut saturated = 100;
    let repair = state_repaired_error("repaired".into());
    assert!(take_repaired_retry(&repair, &mut available));
    assert!(!take_repaired_retry(&repair, &mut available));
    assert!(!take_repaired_retry(&io::Error::other("unchanged failure"), &mut true));
    assert!(next_reconnect_delay(&mut saturated) >= Duration::from_secs(27));
}

#[tokio::test]
async fn deferred_ingress_keeps_ack_effects_and_cursor_frontier_separate() {
    let (client, mut peer) = connected_websocket_pair().await;
    let (_writer, reader) = client.split();
    let (state, _used) = writer_state();
    state.lock().await.outbound_buffer.insert(&server_envelope(&ClientId("client".into()), "stream", 1, "retained"));
    let (outgoing, _outgoing_rx) = mpsc::channel(128);
    let (events, mut event_rx) = mpsc::channel(2);
    let shutdown = CancellationToken::new();
    let tracker = Arc::new(Mutex::new(ClientTracker::new(outgoing, events.clone(), &shutdown)));
    let task = tokio::spawn(RemoteControlWebsocket::run_websocket_reader_inner(
        tracker, state.clone(), reader, Duration::from_secs(60), shutdown.clone(),
    ));
    peer.send(tungstenite::Message::Text(serde_json::json!({
        "type":"client_message", "client_id":"client", "stream_id":"stream", "seq_id":0, "cursor":"initial",
        "message":{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test","version":"1"}}}
    }).to_string().into())).await.unwrap();
    let (connection_id, _connection_writer) = match event_rx.recv().await.unwrap() {
        TransportEvent::ConnectionOpened { connection_id, writer, .. } => (connection_id, writer),
        other => panic!("unexpected {other:?}"),
    };
    event_rx.recv().await.unwrap();
    for _ in 0..100 {
        if state.lock().await.subscribe_cursor.as_deref() == Some("initial") { break; }
        tokio::task::yield_now().await;
    }
    assert_eq!(state.lock().await.subscribe_cursor.as_deref(), Some("initial"));
    for _ in 0..2 { events.try_send(TransportEvent::ConnectionClosed { connection_id }).unwrap(); }
    tokio::time::pause();
    peer.send(tungstenite::Message::Text(serde_json::json!({
        "type":"client_message","client_id":"client","stream_id":"stream","seq_id":1,"cursor":"blocked",
        "message":{"jsonrpc":"2.0","method":"initialized"}
    }).to_string().into())).await.unwrap();
    peer.send(tungstenite::Message::Text(serde_json::json!({
        "type":"ack","client_id":"client","stream_id":"stream","seq_id":1,"cursor":"later-ack"
    }).to_string().into())).await.unwrap();
    peer.send(tungstenite::Message::Pong(Vec::new().into())).await.unwrap();
    for _ in 0..1000 {
        if state.lock().await.outbound_buffer.server_envelopes().count() == 0 { break; }
        tokio::task::yield_now().await;
    }
    assert_eq!(state.lock().await.outbound_buffer.server_envelopes().count(), 0, "ACK must not wait for ingress timeout");
    assert_eq!(state.lock().await.subscribe_cursor.as_deref(), Some("initial"), "later ACK cannot skip deferred application message on reconnect");
    shutdown.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(state.lock().await.subscribe_cursor.as_deref(), Some("initial"));
}

fn initialize_json(id: u64) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{"clientInfo":{"name":"test","version":"1"}}})
}

async fn send_client_json(peer: &mut WebSocketStream<TcpStream>, value: serde_json::Value) {
    peer.send(tungstenite::Message::Text(value.to_string().into())).await.unwrap();
}

fn initialize_chunk(seq_id: u64, segment_id: usize, count: usize, raw: &[u8], chunk: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "type":"client_message_chunk","client_id":"client","stream_id":"stream","seq_id":seq_id,
        "segment_id":segment_id,"segment_count":count,"message_size_bytes":raw.len(),
        "message_chunk_base64":base64::engine::general_purpose::STANDARD.encode(chunk),
    })
}

async fn ack_reader_barrier(peer: &mut WebSocketStream<TcpStream>, state: &Mutex<WebsocketState>, cursor: &str) {
    state.lock().await.outbound_buffer.insert(&server_envelope(&ClientId("control".into()), "stream", 1, "barrier"));
    send_client_json(peer, serde_json::json!({
        "type":"ack","client_id":"control","stream_id":"stream","seq_id":1,"cursor":cursor,
    })).await;
    for _ in 0..1000 {
        if state.lock().await.outbound_buffer.server_envelopes().count() == 0 { return; }
        tokio::task::yield_now().await;
    }
    panic!("reader must consume earlier frames without awaiting ingress");
}

async fn wait_for_cursor(state: &Mutex<WebsocketState>, expected: &str) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if state.lock().await.subscribe_cursor.as_deref() == Some(expected) { break; }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.expect("admitted prefix cursor advances");
}

#[tokio::test]
async fn deferred_chunk_initialize_dedupes_before_admission_and_retries_after_failure() {
    for fail_first in [false, true] {
        let (state, _used) = writer_state();
        let (outgoing, _outgoing_rx) = mpsc::channel(128);
        let (events, mut event_rx) = mpsc::channel(3);
        let root = CancellationToken::new();
        let tracker = Arc::new(Mutex::new(ClientTracker::new(outgoing, events.clone(), &root)));
        let raw = serde_json::to_vec(&initialize_json(1)).unwrap();
        let attempts = if fail_first { 2 } else { 1 };
        for attempt in 0..attempts {
            let (client, mut peer) = connected_websocket_pair().await;
            let (_writer, reader) = client.split();
            for _ in 0..3 {
                events.try_send(TransportEvent::ConnectionClosed { connection_id: crate::transport::next_connection_id() }).unwrap();
            }
            let shutdown = root.child_token();
            let task = tokio::spawn(RemoteControlWebsocket::run_websocket_reader_inner(
                tracker.clone(), state.clone(), reader, Duration::from_secs(60), shutdown.clone(),
            ));
            tokio::time::pause();
            for _ in 0..2 { send_client_json(&mut peer, initialize_chunk(7, 0, 1, &raw, &raw)).await; }
            ack_reader_barrier(&mut peer, &state, "after-duplicates").await;
            assert!(state.lock().await.subscribe_cursor.is_none());
            if fail_first && attempt == 0 {
                tokio::time::advance(Duration::from_millis(20)).await;
                assert!(task.await.unwrap().is_err(), "full ingress must reject without committing dedupe");
                for _ in 0..3 { event_rx.recv().await.unwrap(); }
                assert!(state.lock().await.subscribe_cursor.is_none());
                tokio::time::resume();
                continue;
            }
            for _ in 0..3 { event_rx.recv().await.unwrap(); }
            let (connection_id, _writer) = match event_rx.recv().await.unwrap() {
                TransportEvent::ConnectionOpened { connection_id, writer, .. } => (connection_id, writer),
                other => panic!("expected one open, got {other:?}"),
            };
            assert!(matches!(event_rx.recv().await, Some(TransportEvent::IncomingMessage { connection_id: id, .. }) if id == connection_id));
            wait_for_cursor(&state, "after-duplicates").await;
            assert!(matches!(event_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)), "duplicate initialize must not close or reopen");
            // A distinct initialize sequence is a real replacement, not a duplicate.
            let replacement = serde_json::to_vec(&initialize_json(2)).unwrap();
            send_client_json(&mut peer, initialize_chunk(8, 0, 1, &replacement, &replacement)).await;
            assert!(matches!(event_rx.recv().await, Some(TransportEvent::ConnectionClosed { connection_id: id }) if id == connection_id));
            let (_replacement_writer, replacement_id) = match event_rx.recv().await.unwrap() {
                TransportEvent::ConnectionOpened { writer, connection_id, .. } => (writer, connection_id),
                other => panic!("expected replacement open, got {other:?}"),
            };
            assert_ne!(replacement_id, connection_id);
            assert!(matches!(event_rx.recv().await, Some(TransportEvent::IncomingMessage { connection_id: id, .. }) if id == replacement_id));
            shutdown.cancel();
            task.await.unwrap().unwrap();
            tokio::time::resume();
        }
    }
}

#[tokio::test]
async fn deferred_close_preserves_later_reinitialize_prefix_in_wire_order() {
    for (legacy_close, pending_old_chunk) in [(false, false), (true, false), (false, true)] {
        let (client, mut peer) = connected_websocket_pair().await;
        let (_writer, reader) = client.split();
        let (state, _used) = writer_state();
        let (outgoing, _outgoing_rx) = mpsc::channel(128);
        let (events, mut event_rx) = mpsc::channel(3);
        let shutdown = CancellationToken::new();
        let tracker = Arc::new(Mutex::new(ClientTracker::new(outgoing, events.clone(), &shutdown)));
        let task = tokio::spawn(RemoteControlWebsocket::run_websocket_reader_inner(
            tracker, state.clone(), reader, Duration::from_secs(60), shutdown.clone(),
        ));
        let stream = if legacy_close { serde_json::Value::Null } else { serde_json::json!("stream") };
        let mut initial_writer = None;
        let mut connection_id = crate::transport::next_connection_id();
        if !pending_old_chunk {
            send_client_json(&mut peer, serde_json::json!({
                "type":"client_message","client_id":"client","stream_id":stream,"seq_id":0,"cursor":"initial","message":initialize_json(1),
            })).await;
            match event_rx.recv().await.unwrap() {
                TransportEvent::ConnectionOpened { connection_id: id, writer, .. } => {
                    connection_id = id;
                    initial_writer = Some(writer);
                }
                other => panic!("expected initial open, got {other:?}"),
            }
            event_rx.recv().await.unwrap();
            wait_for_cursor(&state, "initial").await;
        }
        for _ in 0..3 { events.try_send(TransportEvent::ConnectionClosed { connection_id }).unwrap(); }
        tokio::time::pause();
        if pending_old_chunk {
            // Its later receipt must not restore the retired generation's
            // high sequence and reject the replacement's reset sequence.
            let old = serde_json::to_vec(&initialize_json(1)).unwrap();
            send_client_json(&mut peer, initialize_chunk(7, 0, 1, &old, &old)).await;
        }
        send_client_json(&mut peer, serde_json::json!({
            "type":"client_closed","client_id":"client","stream_id":stream,"cursor":"closed",
        })).await;
        let raw = serde_json::to_vec(&initialize_json(2)).unwrap();
        let split = raw.len() / 2;
        send_client_json(&mut peer, initialize_chunk(0, 0, 2, &raw, &raw[..split])).await;
        ack_reader_barrier(&mut peer, &state, "prefix-received").await;
        for _ in 0..3 { event_rx.recv().await.unwrap(); }
        if pending_old_chunk {
            match event_rx.recv().await.unwrap() {
                TransportEvent::ConnectionOpened { connection_id: id, writer, .. } => {
                    connection_id = id;
                    initial_writer = Some(writer);
                }
                other => panic!("expected deferred initial open, got {other:?}"),
            }
            assert!(matches!(event_rx.recv().await, Some(TransportEvent::IncomingMessage { .. })));
        }
        assert!(matches!(event_rx.recv().await, Some(TransportEvent::ConnectionClosed { connection_id: id }) if id == connection_id));
        wait_for_cursor(&state, "prefix-received").await;
        send_client_json(&mut peer, initialize_chunk(0, 1, 2, &raw, &raw[split..])).await;
        let (replacement_id, _replacement_writer) = match tokio::time::timeout(Duration::from_secs(1), event_rx.recv()).await.expect("new assembly prefix survives close admission").unwrap() {
            TransportEvent::ConnectionOpened { connection_id, writer, .. } => (connection_id, writer),
            other => panic!("expected replacement open, got {other:?}"),
        };
        assert_ne!(replacement_id, connection_id);
        assert!(matches!(event_rx.recv().await, Some(TransportEvent::IncomingMessage { connection_id: id, .. }) if id == replacement_id));
        shutdown.cancel();
        task.await.unwrap().unwrap();
        drop(initial_writer);
        tokio::time::resume();
    }
}
