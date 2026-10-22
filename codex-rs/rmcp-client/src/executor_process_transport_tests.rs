use bytes::BytesMut;
use pretty_assertions::assert_eq;

use super::LineBuffer;

const TEST_LINE_LIMIT: usize = 64;

#[test]
fn fragmented_lines_preserve_order_and_the_unterminated_tail() {
    let mut buffer = LineBuffer::default();

    buffer
        .extend_from_slice(b"partial", TEST_LINE_LIMIT)
        .unwrap();
    assert_eq!(buffer.take_line(), None);

    buffer.extend_from_slice(b" line", TEST_LINE_LIMIT).unwrap();
    assert_eq!(buffer.take_line(), None);

    buffer
        .extend_from_slice(b"\nnext\npartial", TEST_LINE_LIMIT)
        .unwrap();
    assert_eq!(
        buffer.take_line(),
        Some(BytesMut::from(&b"partial line"[..]))
    );
    assert_eq!(buffer.take_line(), Some(BytesMut::from(&b"next"[..])));
    assert_eq!(buffer.take_line(), None);
    assert_eq!(
        buffer.take_remaining(),
        Some(BytesMut::from(&b"partial"[..]))
    );
    assert_eq!(buffer.take_remaining(), None);
}

#[test]
fn takes_unterminated_remaining_bytes_at_eof() {
    let mut buffer = LineBuffer::default();
    buffer
        .extend_from_slice(b"remaining", TEST_LINE_LIMIT)
        .unwrap();
    assert_eq!(buffer.take_line(), None);

    assert_eq!(
        buffer.take_remaining(),
        Some(BytesMut::from(&b"remaining"[..]))
    );
    assert_eq!(buffer.take_line(), None);
    assert_eq!(buffer.take_remaining(), None);

    let at_limit = vec![b'x'; TEST_LINE_LIMIT];
    buffer
        .extend_from_slice(&at_limit, TEST_LINE_LIMIT)
        .unwrap();
    buffer.extend_from_slice(b"\n", TEST_LINE_LIMIT).unwrap();
    assert_eq!(
        buffer.take_line(),
        Some(BytesMut::from(at_limit.as_slice()))
    );
    assert_eq!(buffer.take_remaining(), None);
}

#[test]
fn rejected_oversized_append_preserves_the_accepted_line() {
    let mut buffer = LineBuffer::default();
    let at_limit = vec![b'x'; TEST_LINE_LIMIT];
    buffer
        .extend_from_slice(&at_limit, TEST_LINE_LIMIT)
        .unwrap();

    assert!(buffer.extend_from_slice(b"x", TEST_LINE_LIMIT).is_err());
    assert_eq!(buffer.take_line(), None);
    buffer.extend_from_slice(b"\n", TEST_LINE_LIMIT).unwrap();
    assert_eq!(
        buffer.take_line(),
        Some(BytesMut::from(at_limit.as_slice()))
    );
    assert_eq!(buffer.take_remaining(), None);
}

#[test]
fn bounds_each_line_instead_of_the_aggregate_buffer() {
    let mut buffer = LineBuffer::default();
    let lines = b"first line\nsecond line\nthird line\n";

    buffer
        .extend_from_slice(lines, b"second line".len())
        .unwrap();

    assert_eq!(buffer.take_line(), Some(BytesMut::from(&b"first line"[..])));
    assert_eq!(
        buffer.take_line(),
        Some(BytesMut::from(&b"second line"[..]))
    );
    assert_eq!(buffer.take_line(), Some(BytesMut::from(&b"third line"[..])));
    assert_eq!(buffer.take_line(), None);
    assert_eq!(buffer.take_remaining(), None);
}

/// Serves `chunks` as retained stdout output to lag recovery.
struct ReplayProcess {
    id: codex_exec_server::ProcessId,
    wake: tokio::sync::watch::Sender<u64>,
    chunks: Vec<codex_exec_server::ProcessOutputChunk>,
    reads: std::sync::atomic::AtomicUsize,
}

impl ReplayProcess {
    fn new(stdout: Vec<Vec<u8>>) -> Self {
        let (wake, _) = tokio::sync::watch::channel(0);
        Self {
            id: "replay".into(),
            wake,
            reads: std::sync::atomic::AtomicUsize::new(0),
            chunks: stdout
                .into_iter()
                .zip(1..)
                .map(|(chunk, seq)| codex_exec_server::ProcessOutputChunk {
                    seq,
                    stream: codex_exec_server::ExecOutputStream::Stdout,
                    chunk: chunk.into(),
                })
                .collect(),
        }
    }
}

impl codex_exec_server::ExecProcess for ReplayProcess {
    fn process_id(&self) -> &codex_exec_server::ProcessId {
        &self.id
    }
    fn subscribe_wake(&self) -> tokio::sync::watch::Receiver<u64> {
        self.wake.subscribe()
    }
    fn subscribe_events(&self) -> codex_exec_server::ExecProcessEventReceiver {
        codex_exec_server::ExecProcessEventReceiver::empty()
    }
    fn read(
        &self,
        _: Option<u64>,
        _: Option<usize>,
        _: Option<u64>,
    ) -> codex_exec_server::ExecProcessFuture<'_, codex_exec_server::ReadResponse> {
        Box::pin(async move {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(codex_exec_server::ReadResponse {
                chunks: self.chunks.clone(),
                next_seq: self.chunks.len() as u64 + 1,
                exited: false,
                exit_code: None,
                closed: false,
                failure: None,
                output_gap: None,
                sandbox_denied: false,
            })
        })
    }
    fn write(
        &self,
        _: Vec<u8>,
    ) -> codex_exec_server::ExecProcessFuture<'_, codex_exec_server::WriteResponse> {
        panic!("unexpected write")
    }
    fn signal(
        &self,
        _: codex_exec_server::ProcessSignal,
    ) -> codex_exec_server::ExecProcessFuture<'_, ()> {
        panic!("unexpected signal")
    }
    fn terminate(&self) -> codex_exec_server::ExecProcessFuture<'_, ()> {
        panic!("unexpected terminate")
    }
}

#[tokio::test]
async fn initial_forward_gap_recovers_all_synthetic_protocol_records_once() {
    // Synthetic adapter frames, not unsolicited responses from a conforming
    // MCP server. Each literal id is an independent ordering/count oracle.
    let records = (1..=300)
        .map(|id| format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{}}}}\n").into_bytes())
        .collect();
    let process = std::sync::Arc::new(ReplayProcess::new(records));
    let mut transport = super::ExecutorProcessTransport::new(process.clone(), "initial-gap".into());
    let trigger = codex_exec_server::ExecProcessEvent::Output(process.chunks[44].clone());
    assert!(
        transport
            .recover_forward_gap(trigger)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(process.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    for id in 1..=300 {
        let message = transport
            .take_stdout_message(false)
            .expect("every record must be recovered");
        assert_eq!(serde_json::to_value(message).unwrap()["id"], id);
    }
    assert!(transport.take_stdout_message(false).is_none());
    let duplicate = transport
        .recover_forward_gap(codex_exec_server::ExecProcessEvent::Output(
            process.chunks[44].clone(),
        ))
        .await
        .unwrap()
        .unwrap();
    if let codex_exec_server::ExecProcessEvent::Output(chunk) = duplicate {
        transport.push_process_output_if_new(chunk);
    }
    assert!(transport.take_stdout_message(false).is_none());
    assert_eq!(process.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn contiguous_initial_event_needs_no_recovery_read() {
    let process = std::sync::Arc::new(ReplayProcess::new(vec![b"diagnostic".to_vec()]));
    let mut transport = super::ExecutorProcessTransport::new(process.clone(), "contiguous".into());
    assert!(
        transport
            .recover_forward_gap(codex_exec_server::ExecProcessEvent::Output(
                process.chunks[0].clone()
            ))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(process.reads.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lag_recovery_does_not_deliver_messages_after_fatal_stdout_overflow() {
    use rmcp::transport::Transport;
    let process = std::sync::Arc::new(ReplayProcess::new(vec![
        vec![b'x'; super::MCP_STDOUT_MAX_MESSAGE_BYTES + 1],
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n".to_vec(),
    ]));
    let mut transport = super::ExecutorProcessTransport::new(process, "replay".into());
    transport.recover_lagged_events().await.unwrap();
    assert!(transport.receive().await.is_none());
}

#[tokio::test]
async fn stdout_messages_larger_than_a_diagnostic_line_are_delivered() {
    use rmcp::transport::Transport;
    // A screenshot-sized tool result, delivered in the executor's 8 KiB pipe reads.
    let text = "x".repeat(2 * super::MCP_STDERR_MAX_LINE_BYTES);
    let mut line = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "content": [{ "type": "text", "text": text }] },
    }))
    .unwrap();
    line.push(b'\n');
    let process = std::sync::Arc::new(ReplayProcess::new(
        line.chunks(8_192).map(<[u8]>::to_vec).collect(),
    ));
    let mut transport = super::ExecutorProcessTransport::new(process, "replay".into());
    transport.recover_lagged_events().await.unwrap();

    let message = transport
        .receive()
        .await
        .expect("a large well-formed message must not close the transport");
    let message = serde_json::to_value(message).unwrap();
    assert_eq!(message["id"], 1);
    assert_eq!(
        message["result"]["content"][0]["text"].as_str(),
        Some(text.as_str())
    );
    transport.recover_lagged_events().await.unwrap();
    assert!(
        transport.take_stdout_message(false).is_none(),
        "replayed event sequences must not deliver duplicate messages"
    );
}
