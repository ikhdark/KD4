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

struct ReplayProcess {
    id: codex_exec_server::ProcessId,
    wake: tokio::sync::watch::Sender<u64>,
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
        Box::pin(async {
            Ok(codex_exec_server::ReadResponse {
                chunks: vec![
                    codex_exec_server::ProcessOutputChunk {
                        seq: 1,
                        stream: codex_exec_server::ExecOutputStream::Stdout,
                        chunk: vec![b'x'; super::MCP_STDIO_MAX_LINE_BYTES + 1].into(),
                    },
                    codex_exec_server::ProcessOutputChunk {
                        seq: 2,
                        stream: codex_exec_server::ExecOutputStream::Stdout,
                        chunk: b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n"
                            .to_vec()
                            .into(),
                    },
                ],
                next_seq: 3,
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
    let (wake, _) = tokio::sync::watch::channel(0);
    let process = std::sync::Arc::new(ReplayProcess {
        id: "replay".into(),
        wake,
    });
    let mut transport = super::ExecutorProcessTransport::new(process, "replay".into());
    transport.recover_lagged_events().await.unwrap();
    assert!(transport.receive().await.is_none());
}
