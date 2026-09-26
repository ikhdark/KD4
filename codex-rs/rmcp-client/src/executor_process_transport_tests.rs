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
}

impl ReplayProcess {
    fn new(stdout: Vec<Vec<u8>>) -> Self {
        let (wake, _) = tokio::sync::watch::channel(0);
        Self {
            id: "replay".into(),
            wake,
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
            Ok(codex_exec_server::ReadResponse {
                chunks: self.chunks.clone(),
                next_seq: self.chunks.len() as u64 + 1,
                exited: false,
                exit_code: None,
                closed: false,
                failure: None,
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
        message["result"]["content"][0]["text"]
            .as_str()
            .map(str::len),
        Some(text.len())
    );
}
