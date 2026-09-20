use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use super::EncodedFrame;
use super::FramedReader;
use super::FramedWriter;
use super::MAX_FRAME_BYTES;

#[tokio::test]
async fn nested_tool_input_presence_survives_the_host_transport() {
    use super::DelegateRequest;
    use super::DelegateRequestId;
    use super::HostToClient;
    use super::SessionId;
    use crate::CellId;
    use crate::CodeModeNestedToolCall;
    use crate::CodeModeToolKind;
    use codex_protocol::ToolName;

    for input in [
        None,
        Some(json!(null)),
        Some(json!({ "value": null })),
        Some(json!("text")),
    ] {
        let expected = CodeModeNestedToolCall {
            cell_id: CellId::new("cell-input".to_string()),
            parent_tool_call_id: Some("parent-call".to_string()),
            runtime_tool_call_id: "nested-call".to_string(),
            tool_name: ToolName::plain("example"),
            tool_kind: CodeModeToolKind::Function,
            input,
            nested_deadline: None,
        };
        let runtime_json = serde_json::to_value(&expected).expect("encode runtime invocation");
        assert_eq!(runtime_json.get("input"), expected.input.as_ref());
        assert_eq!(
            serde_json::from_value::<CodeModeNestedToolCall>(runtime_json)
                .expect("decode runtime invocation"),
            expected
        );

        let message = HostToClient::DelegateRequest {
            id: DelegateRequestId::new(1),
            session_id: SessionId::new("session-input").expect("valid session ID"),
            request: DelegateRequest::InvokeTool {
                invocation: expected.clone().into(),
            },
        };
        let wire_json = serde_json::to_value(&message).expect("encode wire invocation");
        assert_eq!(
            wire_json["request"]["invocation"].get("input"),
            expected.input.as_ref()
        );

        let (writer, reader) = tokio::io::duplex(128);
        let write = tokio::spawn(async move {
            FramedWriter::new(writer)
                .write(&message)
                .await
                .expect("write invocation frame");
        });
        let received = FramedReader::new(reader)
            .read::<HostToClient>()
            .await
            .expect("read invocation frame")
            .expect("invocation frame present");
        write.await.expect("writer task");
        let HostToClient::DelegateRequest {
            request: DelegateRequest::InvokeTool { invocation },
            ..
        } = received
        else {
            panic!("expected nested tool invocation");
        };
        assert_eq!(CodeModeNestedToolCall::from(invocation), expected);
    }
}

#[cfg(test)]
mod nested_deadline_transport {
    use super::super::WireNestedToolCall;
    use crate::CellId;
    use crate::CodeModeNestedToolCall;
    use crate::CodeModeToolKind;
    use codex_protocol::ToolName;
    use std::time::Duration;
    use std::time::Instant;

    fn invocation(nested_deadline: Option<Instant>) -> CodeModeNestedToolCall {
        CodeModeNestedToolCall {
            cell_id: CellId::new("cell-deadline".to_string()),
            parent_tool_call_id: Some("parent-call".to_string()),
            runtime_tool_call_id: "nested-call".to_string(),
            tool_name: ToolName::plain("example"),
            tool_kind: CodeModeToolKind::Function,
            input: None,
            nested_deadline,
        }
    }

    /// The receiver must recover the sender's remaining budget, not restart it.
    #[test]
    fn a_delayed_delivery_is_charged_against_the_original_budget() {
        let sent = invocation(Some(Instant::now() + Duration::from_millis(1_000)));
        let wire = WireNestedToolCall::from(sent);
        assert!(
            wire.deadline_shared_monotonic_nanos.is_some(),
            "a supported platform must send the shared monotonic form"
        );

        // Stand in for transit time between the two processes.
        std::thread::sleep(Duration::from_millis(250));

        let received = CodeModeNestedToolCall::from(wire)
            .nested_deadline
            .expect("the deadline must survive the crossing");
        let remaining = received.saturating_duration_since(Instant::now());
        assert!(
            remaining < Duration::from_millis(900),
            "transit must be charged to the budget, not refunded: {remaining:?} left of 1000ms"
        );
        assert!(
            remaining > Duration::from_millis(400),
            "only the elapsed transit may be charged: {remaining:?} left of 1000ms"
        );
    }

    /// The exchanged value is a monotonic reading, so re-reading the encoded
    /// frame later cannot be moved by a wall-clock adjustment: the recovered
    /// deadline depends only on the monotonic offset.
    #[test]
    fn a_wall_clock_adjustment_between_send_and_receipt_does_not_move_the_deadline() {
        let wire = WireNestedToolCall::from(invocation(Some(
            Instant::now() + Duration::from_millis(1_000),
        )));
        let monotonic_deadline = wire
            .deadline_shared_monotonic_nanos
            .expect("shared monotonic form");

        // A wall-clock jump changes SystemTime but not the monotonic source the
        // deadline is expressed on, so the same frame still decodes to the same
        // remaining budget.
        let first = CodeModeNestedToolCall::from(wire.clone())
            .nested_deadline
            .expect("deadline");
        let second = CodeModeNestedToolCall::from(wire.clone())
            .nested_deadline
            .expect("deadline");
        let drift = second.saturating_duration_since(first);
        assert!(
            drift < Duration::from_millis(50),
            "two receipts of one frame must agree on the deadline, drifted {drift:?}"
        );
        assert_eq!(
            wire.deadline_shared_monotonic_nanos,
            Some(monotonic_deadline),
            "encoding must not mutate the monotonic reading"
        );
    }

    /// Without a shared source the fallback is charged from receipt. It does
    /// not preserve the original budget, and that is the documented contract.
    #[test]
    fn the_fallback_charges_the_remaining_duration_from_receipt() {
        let wire = WireNestedToolCall {
            deadline_shared_monotonic_nanos: None,
            remaining_ms_at_send: Some(1_000),
            ..WireNestedToolCall::from(invocation(None))
        };

        let received = CodeModeNestedToolCall::from(wire)
            .nested_deadline
            .expect("the fallback still yields a deadline");
        let remaining = received.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::from_millis(900) && remaining <= Duration::from_millis(1_000),
            "the fallback restarts the budget at receipt: {remaining:?}"
        );
    }

    #[test]
    fn an_invocation_without_a_deadline_sends_and_receives_none() {
        let wire = WireNestedToolCall::from(invocation(None));
        assert_eq!(wire.deadline_shared_monotonic_nanos, None);
        assert_eq!(wire.remaining_ms_at_send, None);
        assert_eq!(CodeModeNestedToolCall::from(wire).nested_deadline, None);
    }
}

#[tokio::test]
async fn frame_write_flushes_exact_wire_bytes_before_the_writer_is_dropped() {
    let (writer, mut reader) = tokio::io::duplex(/*max_buf_size*/ 128);
    let mut writer = FramedWriter::new(tokio::io::BufWriter::new(writer));
    writer
        .write(&json!({"value": 1}))
        .await
        .expect("write frame");
    let mut bytes = [0; 15];
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        reader.read_exact(&mut bytes),
    )
    .await
    .expect("frame must be delivered while the buffered writer is alive")
    .expect("read bytes");
    assert_eq!(&bytes, b"\x0b\0\0\0{\"value\":1}");
    drop(writer);
}

#[tokio::test]
async fn encoded_frame_serializes_once_and_reuses_the_encoded_bytes() {
    use std::cell::Cell;

    use serde::Serialize;

    struct CountingMessage(Cell<u8>);

    impl Serialize for CountingMessage {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let calls = self.0.get() + 1;
            self.0.set(calls);
            serializer.serialize_u8(calls)
        }
    }

    let message = CountingMessage(Cell::new(0));
    let frame = EncodedFrame::encode(&message).expect("encode once");
    assert_eq!(message.0.get(), 1);
    let mut bytes = Vec::new();
    let mut writer = FramedWriter::new(&mut bytes);
    writer.write_frame(&frame).await.expect("first delivery");
    writer.write_frame(&frame).await.expect("second delivery");
    assert_eq!(message.0.get(), 1);
    assert_eq!(bytes, b"\x01\0\0\x001\x01\0\0\x001");
}

#[tokio::test]
async fn serializer_failure_after_partial_json_writes_no_transport_bytes() {
    use serde::Serialize;
    use serde::ser::Error;
    use serde::ser::SerializeSeq;

    struct FailingMessage;

    impl Serialize for FailingMessage {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let mut sequence = serializer.serialize_seq(Some(2))?;
            sequence.serialize_element("partial JSON")?;
            Err(S::Error::custom("deliberate serialization failure"))
        }
    }

    let mut bytes = Vec::new();
    let mut writer = FramedWriter::new(&mut bytes);
    let err = writer
        .write(&FailingMessage)
        .await
        .expect_err("serializer failure");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("deliberate serialization failure"));
    writer
        .write(&7)
        .await
        .expect("reuse after encoding failure");
    assert_eq!(bytes, b"\x01\0\0\x007");
}

#[tokio::test]
async fn cancelled_wait_can_resume_the_same_partial_frame_read() {
    use std::future::Future;
    use std::task::Context;
    use std::task::Waker;

    use tokio_util::sync::CancellationToken;

    let bytes = b"\x0b\0\0\0{\"value\":1}";
    for split in [1, 3, 4, 7] {
        let (mut writer, reader) = tokio::io::duplex(32);
        let mut reader = FramedReader::new(reader);
        writer
            .write_all(&bytes[..split])
            .await
            .expect("partial frame");
        let mut read = Box::pin(reader.read::<serde_json::Value>());
        assert!(
            read.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        tokio::select! {
            biased;
            () = cancelled.cancelled() => {}
            result = &mut read => panic!("partial read completed early: {result:?}"),
        }
        writer
            .write_all(&bytes[split..])
            .await
            .expect("remaining frame");
        assert_eq!(
            read.await.expect("resume original read"),
            Some(json!({"value": 1}))
        );
        drop(writer);
        assert_eq!(
            reader
                .read::<serde_json::Value>()
                .await
                .expect("boundary EOF"),
            None
        );
    }
}

#[tokio::test]
async fn cancelled_wait_can_resume_the_same_partial_frame_write() {
    use std::future::Future;
    use std::task::Context;
    use std::task::Waker;

    use tokio_util::sync::CancellationToken;

    let frame = EncodedFrame::encode(&json!({"value": 1})).expect("encode frame");
    for split in [1, 3, 4, 7] {
        let (writer, mut reader) = tokio::io::duplex(split);
        let mut writer = FramedWriter::new(writer);
        let mut write = Box::pin(writer.write_frame(&frame));
        assert!(
            write
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        tokio::select! {
            biased;
            () = cancelled.cancelled() => {}
            result = &mut write => panic!("partial write completed early: {result:?}"),
        }
        let mut bytes = [0; 15];
        reader
            .read_exact(&mut bytes[..split])
            .await
            .expect("already written prefix");
        let (written, read) = tokio::join!(write, reader.read_exact(&mut bytes[split..]));
        written.expect("resume original write");
        read.expect("remaining frame");
        assert_eq!(&bytes, b"\x0b\0\0\0{\"value\":1}");
    }
}

#[tokio::test]
async fn frame_reader_accepts_only_the_tag_only_notification_acknowledgment() {
    use super::ClientToHost;
    use super::DelegateRequestId;
    use super::DelegateResponse;
    use super::WireResult;

    let valid = br#"{"type":"delegate/response","id":7,"result":{"status":"ok","value":{"type":"notification/delivered"}}}"#;
    let invalid = br#"{"type":"delegate/response","id":7,"result":{"status":"ok","value":{"type":"notification/delivered","unexpected":true}}}"#;
    let mut bytes = Vec::new();
    for payload in [invalid.as_slice(), valid.as_slice()] {
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
    }
    let mut reader = FramedReader::new(bytes.as_slice());
    let err = reader
        .read::<ClientToHost>()
        .await
        .expect_err("extra acknowledgment field");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        reader
            .read::<ClientToHost>()
            .await
            .expect("next valid frame"),
        Some(ClientToHost::DelegateResponse {
            id: DelegateRequestId::new(7),
            result: WireResult::Ok {
                value: DelegateResponse::NotificationDelivered {}
            },
        })
    );
    assert_eq!(
        reader.read::<ClientToHost>().await.expect("boundary EOF"),
        None
    );
}

#[tokio::test]
async fn fragmented_frame_round_trips() {
    let value = json!({"type": "session/open", "sessionId": "session-1"});
    let payload = serde_json::to_vec(&value).expect("serialize");
    let mut bytes = (payload.len() as u32).to_le_bytes().to_vec();
    bytes.extend(payload);

    let (mut writer, reader) = tokio::io::duplex(/*max_buf_size*/ 1);
    let write = tokio::spawn(async move {
        for byte in bytes {
            writer.write_all(&[byte]).await.expect("write byte");
        }
    });

    assert_eq!(
        FramedReader::new(reader)
            .read::<serde_json::Value>()
            .await
            .expect("read frame"),
        Some(value)
    );
    write.await.expect("writer task");
}

#[tokio::test]
async fn eof_is_clean_only_at_a_frame_boundary() {
    let (writer, reader) = tokio::io::duplex(/*max_buf_size*/ 16);
    drop(writer);
    assert_eq!(
        FramedReader::new(reader)
            .read::<serde_json::Value>()
            .await
            .expect("clean eof"),
        None
    );

    let (mut writer, reader) = tokio::io::duplex(/*max_buf_size*/ 16);
    writer
        .write_all(&[1, 0])
        .await
        .expect("write partial header");
    drop(writer);
    let err = FramedReader::new(reader)
        .read::<serde_json::Value>()
        .await
        .expect_err("truncated header");
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);

    let (mut writer, reader) = tokio::io::duplex(/*max_buf_size*/ 16);
    writer
        .write_all(&4_u32.to_le_bytes())
        .await
        .expect("header");
    writer.write_all(b"{}").await.expect("partial payload");
    drop(writer);
    let err = FramedReader::new(reader)
        .read::<serde_json::Value>()
        .await
        .expect_err("truncated payload");
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn consecutive_frames_end_at_a_clean_boundary() {
    let (writer, reader) = tokio::io::duplex(/*max_buf_size*/ 16);
    let write = tokio::spawn(async move {
        let mut writer = FramedWriter::new(writer);
        writer
            .write(&json!({"first": 1}))
            .await
            .expect("first frame");
        writer
            .write(&json!(["second", 2]))
            .await
            .expect("second frame");
    });
    let mut reader = FramedReader::new(reader);
    assert_eq!(
        reader
            .read::<serde_json::Value>()
            .await
            .expect("first frame"),
        Some(json!({"first": 1}))
    );
    assert_eq!(
        reader
            .read::<serde_json::Value>()
            .await
            .expect("second frame"),
        Some(json!(["second", 2]))
    );
    assert_eq!(
        reader
            .read::<serde_json::Value>()
            .await
            .expect("boundary EOF"),
        None
    );
    write.await.expect("writer task");
}

#[tokio::test]
async fn oversized_encoding_stops_early_and_leaves_the_stream_usable() {
    use std::cell::Cell;

    use serde::Serialize;
    use serde::ser::SerializeSeq;

    struct OversizedMessage {
        serialized_chunks: Cell<usize>,
    }

    impl Serialize for OversizedMessage {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let chunk = "x".repeat(1024);
            let mut sequence = serializer.serialize_seq(None)?;
            for _ in 0..=MAX_FRAME_BYTES / 1024 {
                self.serialized_chunks.set(self.serialized_chunks.get() + 1);
                sequence.serialize_element(&chunk)?;
            }
            sequence.end()
        }
    }

    let message = OversizedMessage {
        serialized_chunks: Cell::new(0),
    };
    let mut bytes = Vec::new();
    let err = FramedWriter::new(&mut bytes)
        .write(&message)
        .await
        .expect_err("oversized encoding");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(message.serialized_chunks.get() > 0);
    assert!(message.serialized_chunks.get() < MAX_FRAME_BYTES / 1024 + 1);
    assert!(
        bytes.is_empty(),
        "encoding failure must not write a frame prefix"
    );

    FramedWriter::new(&mut bytes)
        .write(&json!({"after": "rejection"}))
        .await
        .expect("write after encoding failure");
    let mut reader = FramedReader::new(bytes.as_slice());
    assert_eq!(
        reader
            .read::<serde_json::Value>()
            .await
            .expect("valid frame"),
        Some(json!({"after": "rejection"}))
    );
    assert_eq!(
        reader
            .read::<serde_json::Value>()
            .await
            .expect("boundary EOF"),
        None
    );
}

#[tokio::test]
async fn oversized_and_malformed_frames_are_rejected() {
    let (mut writer, reader) = tokio::io::duplex(/*max_buf_size*/ 16);
    writer
        .write_all(&((MAX_FRAME_BYTES as u32) + 1).to_le_bytes())
        .await
        .expect("write oversized header");
    let err = FramedReader::new(reader)
        .read::<serde_json::Value>()
        .await
        .expect_err("oversized frame");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

    let (mut writer, reader) = tokio::io::duplex(/*max_buf_size*/ 16);
    writer
        .write_all(&(1_u32).to_le_bytes())
        .await
        .expect("write length");
    writer.write_all(b"{").await.expect("write malformed json");
    let err = FramedReader::new(reader)
        .read::<serde_json::Value>()
        .await
        .expect_err("malformed frame");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}
