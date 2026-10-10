use super::protocol::ClientEnvelope;
use super::protocol::ClientEvent;
use super::protocol::ClientId;
use super::protocol::ServerEnvelope;
use super::protocol::ServerEvent;
use super::protocol::StreamId;
use super::segment::ClientSegmentObservation;
use super::segment::ClientSegmentReassembler;
use super::segment::REMOTE_CONTROL_SEGMENT_MAX_BYTES;
use super::segment::split_server_envelope_for_transport;
use crate::outgoing_message::OutgoingMessage;
use base64::Engine;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::ServerNotification;
use pretty_assertions::assert_eq;

#[test]
fn reassembles_client_message_chunks() {
    let message = JSONRPCMessage::Notification(JSONRPCNotification {
        method: "initialized".to_string(),
        params: None,
    });
    let raw = serde_json::to_vec(&message).expect("message should serialize");
    let split = raw.len() / 2;
    let client_id = ClientId("client-1".to_string());
    let stream_id = Some(StreamId("stream-1".to_string()));
    let mut reassembler = ClientSegmentReassembler::default();

    assert!(matches!(
        reassembler.observe(chunk_envelope(
            client_id.clone(),
            stream_id.clone(),
            /*seq_id*/ 7,
            /*segment_id*/ 0,
            /*segment_count*/ 2,
            raw.len(),
            &raw[..split],
        )),
        ClientSegmentObservation::Pending
    ));
    let reassembled = match reassembler.observe(chunk_envelope(
        client_id.clone(),
        stream_id,
        /*seq_id*/ 7,
        /*segment_id*/ 1,
        /*segment_count*/ 2,
        raw.len(),
        &raw[split..],
    )) {
        ClientSegmentObservation::Forward(reassembled) => *reassembled,
        ClientSegmentObservation::Pending | ClientSegmentObservation::Dropped => {
            panic!("message should reassemble")
        }
    };
    assert_eq!(reassembled.client_id, client_id);
    assert_eq!(
        reassembled.stream_id,
        Some(StreamId("stream-1".to_string()))
    );
    assert_eq!(reassembled.seq_id, Some(7));
    assert_eq!(reassembled.cursor, None);
    match reassembled.event {
        ClientEvent::ClientMessage {
            message: reassembled_message,
        } => assert_eq!(reassembled_message, message),
        other => panic!("expected client message, got {other:?}"),
    }
}

#[test]
fn splits_large_server_messages_into_wire_chunks() {
    let envelope = ServerEnvelope {
        event: ServerEvent::ServerMessage {
            message: Box::new(OutgoingMessage::AppServerNotification(
                ServerNotification::ConfigWarning(ConfigWarningNotification {
                    summary: "x".repeat(REMOTE_CONTROL_SEGMENT_MAX_BYTES),
                    details: None,
                    path: None,
                    range: None,
                }),
            )),
        },
        client_id: ClientId("client-1".to_string()),
        stream_id: StreamId("stream-1".to_string()),
        seq_id: 9,
    };

    let ServerEvent::ServerMessage { message } = &envelope.event else {
        unreachable!()
    };
    let expected = serde_json::to_vec(message).expect("message should serialize");
    let segments = split_server_envelope_for_transport(envelope).expect("split should succeed");

    assert!(segments.len() > 1);
    assert!(
        segments
            .iter()
            .all(|segment| matches!(segment.event, ServerEvent::ServerMessageChunk { .. }))
    );
    assert!(segments.iter().all(|segment| segment.seq_id == 9));
    assert!(segments.iter().all(|segment| {
        serde_json::to_vec(segment)
            .expect("segment should serialize")
            .len()
            <= REMOTE_CONTROL_SEGMENT_MAX_BYTES
    }));
    let mut reconstructed = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        assert_eq!(segment.client_id, ClientId("client-1".to_string()));
        assert_eq!(segment.stream_id, StreamId("stream-1".to_string()));
        let ServerEvent::ServerMessageChunk {
            segment_id,
            segment_count,
            message_size_bytes,
            message_chunk_base64,
        } = &segment.event
        else {
            panic!("expected chunk")
        };
        assert_eq!(*segment_id, index);
        assert_eq!(*segment_count, segments.len());
        assert_eq!(*message_size_bytes, expected.len());
        reconstructed.extend(
            base64::engine::general_purpose::STANDARD
                .decode(message_chunk_base64)
                .expect("valid base64"),
        );
    }
    assert_eq!(reconstructed, expected);
}

#[test]
fn invalidates_incomplete_stream_assemblies() {
    let message = JSONRPCMessage::Notification(JSONRPCNotification {
        method: "initialized".to_string(),
        params: None,
    });
    let raw = serde_json::to_vec(&message).expect("message should serialize");
    let split = raw.len() / 2;
    let client_id = ClientId("client-1".to_string());
    let stream_id = StreamId("stream-1".to_string());
    let mut reassembler = ClientSegmentReassembler::default();

    assert!(matches!(
        reassembler.observe(chunk_envelope(
            client_id.clone(),
            Some(stream_id.clone()),
            /*seq_id*/ 7,
            /*segment_id*/ 0,
            /*segment_count*/ 2,
            raw.len(),
            &raw[..split],
        )),
        ClientSegmentObservation::Pending
    ));
    reassembler.invalidate_stream(&client_id, &stream_id);
    assert!(matches!(
        reassembler.observe(chunk_envelope(
            client_id,
            Some(stream_id),
            /*seq_id*/ 7,
            /*segment_id*/ 1,
            /*segment_count*/ 2,
            raw.len(),
            &raw[split..],
        )),
        ClientSegmentObservation::Dropped
    ));
}

#[test]
fn resets_incomplete_client_assembly_when_stream_changes() {
    let message = JSONRPCMessage::Notification(JSONRPCNotification {
        method: "initialized".to_string(),
        params: None,
    });
    let raw = serde_json::to_vec(&message).expect("message should serialize");
    let split = raw.len() / 2;
    let client_id = ClientId("client-1".to_string());
    let first_stream_id = StreamId("stream-1".to_string());
    let second_stream_id = StreamId("stream-2".to_string());
    let mut reassembler = ClientSegmentReassembler::default();

    assert!(matches!(
        reassembler.observe(chunk_envelope(
            client_id.clone(),
            Some(first_stream_id.clone()),
            /*seq_id*/ 7,
            /*segment_id*/ 0,
            /*segment_count*/ 2,
            raw.len(),
            &raw[..split],
        )),
        ClientSegmentObservation::Pending
    ));
    assert!(matches!(
        reassembler.observe(chunk_envelope(
            client_id.clone(),
            Some(second_stream_id.clone()),
            /*seq_id*/ 8,
            /*segment_id*/ 0,
            /*segment_count*/ 2,
            raw.len(),
            &raw[..split],
        )),
        ClientSegmentObservation::Pending
    ));
    let reassembled = match reassembler.observe(chunk_envelope(
        client_id.clone(),
        Some(second_stream_id),
        /*seq_id*/ 8,
        /*segment_id*/ 1,
        /*segment_count*/ 2,
        raw.len(),
        &raw[split..],
    )) {
        ClientSegmentObservation::Forward(reassembled) => *reassembled,
        ClientSegmentObservation::Pending | ClientSegmentObservation::Dropped => {
            panic!("replacement stream should reassemble")
        }
    };
    assert_eq!(
        reassembled.stream_id,
        Some(StreamId("stream-2".to_string()))
    );
    assert_eq!(reassembled.client_id, client_id);
    assert_eq!(reassembled.seq_id, Some(8));
    let ClientEvent::ClientMessage { message: actual } = reassembled.event else {
        panic!("replacement chunks must become a client message");
    };
    assert_eq!(actual, message);
    assert!(matches!(
        reassembler.observe(chunk_envelope(
            client_id,
            Some(first_stream_id),
            /*seq_id*/ 7,
            /*segment_id*/ 1,
            /*segment_count*/ 2,
            raw.len(),
            &raw[split..],
        )),
        ClientSegmentObservation::Dropped
    ));
}

#[test]
fn stale_and_duplicate_chunks_preserve_the_current_message() {
    let message = JSONRPCMessage::Notification(JSONRPCNotification {
        method: "initialized".to_string(),
        params: None,
    });
    let raw = serde_json::to_vec(&message).expect("message should serialize");
    let split = raw.len() / 2;
    for (seq_id, segment_id, chunk) in [(7, 0, &raw[..split]), (7, 1, &b""[..]), (8, 0, &b""[..])] {
        let client_id = ClientId("client-1".to_string());
        let stream_id = Some(StreamId("stream-1".to_string()));
        let mut reassembler = ClientSegmentReassembler::default();
        assert!(matches!(
            reassembler.observe(chunk_envelope(
                client_id.clone(), stream_id.clone(), 8, 0, 2, raw.len(), &raw[..split],
            )),
            ClientSegmentObservation::Pending
        ));
        assert!(matches!(
            reassembler.observe(chunk_envelope(
                client_id.clone(), stream_id.clone(), seq_id, segment_id, 2, raw.len(), chunk,
            )),
            ClientSegmentObservation::Dropped
        ));
        let ClientSegmentObservation::Forward(reassembled) = reassembler.observe(chunk_envelope(
            client_id.clone(), stream_id.clone(), 8, 1, 2, raw.len(), &raw[split..],
        )) else {
            panic!("current message must survive seq={seq_id} segment={segment_id}");
        };
        assert_eq!(reassembled.client_id, client_id);
        assert_eq!(reassembled.stream_id, stream_id);
        assert_eq!(reassembled.seq_id, Some(8));
        assert_eq!(reassembled.cursor, None);
        let ClientEvent::ClientMessage { message: actual } = reassembled.event else {
            panic!("completed chunks must become a client message");
        };
        assert_eq!(actual, message);
    }
}

fn chunk_envelope(
    client_id: ClientId,
    stream_id: Option<StreamId>,
    seq_id: u64,
    segment_id: usize,
    segment_count: usize,
    message_size_bytes: usize,
    chunk: &[u8],
) -> ClientEnvelope {
    ClientEnvelope {
        event: ClientEvent::ClientMessageChunk {
            segment_id,
            segment_count,
            message_size_bytes,
            message_chunk_base64: base64::engine::general_purpose::STANDARD.encode(chunk),
        },
        client_id,
        stream_id,
        seq_id: Some(seq_id),
        cursor: None,
    }
}

#[test]
fn unsendable_server_message_is_an_error() {
    let envelope = ServerEnvelope {
        event: ServerEvent::ServerMessage {
            message: Box::new(OutgoingMessage::AppServerNotification(
                ServerNotification::ConfigWarning(ConfigWarningNotification {
                    summary: "warning".to_string(),
                    details: None,
                    path: None,
                    range: None,
                }),
            )),
        },
        client_id: ClientId("x".repeat(REMOTE_CONTROL_SEGMENT_MAX_BYTES)),
        stream_id: StreamId("stream".to_string()),
        seq_id: 1,
    };
    assert_eq!(
        split_server_envelope_for_transport(envelope)
            .expect_err("metadata cannot fit")
            .kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn splits_server_messages_when_chunk_sizes_plateau() {
    let message = OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
        ConfigWarningNotification {
            summary: "x".repeat(1024),
            details: None,
            path: None,
            range: None,
        },
    ));
    let raw = serde_json::to_vec(&message).expect("message should serialize");
    let max_count = super::segment::REMOTE_CONTROL_SEGMENT_COUNT_MAX;
    assert!(raw.len() > max_count);
    let mut witness = ServerEnvelope {
        event: ServerEvent::ServerMessageChunk {
            segment_id: max_count - 1,
            segment_count: max_count,
            message_size_bytes: raw.len(),
            message_chunk_base64: String::new(),
        },
        client_id: ClientId(String::new()),
        stream_id: StreamId("stream-1".to_string()),
        seq_id: 9,
    };
    // Reserve 16 base64 bytes: independently demonstrate a legal partition
    // into 12-byte chunks, even with nearly frame-sized routing metadata.
    let metadata_size = serde_json::to_vec(&witness).expect("metadata should serialize").len();
    witness.client_id = ClientId("x".repeat(REMOTE_CONTROL_SEGMENT_MAX_BYTES - metadata_size - 16));
    let count = raw.len().div_ceil(12);
    assert!(count <= max_count);
    for (segment_id, chunk) in raw.chunks(12).enumerate() {
        witness.event = ServerEvent::ServerMessageChunk {
            segment_id,
            segment_count: count,
            message_size_bytes: raw.len(),
            message_chunk_base64: base64::engine::general_purpose::STANDARD.encode(chunk),
        };
        assert!(serde_json::to_vec(&witness).expect("witness should serialize").len()
            <= REMOTE_CONTROL_SEGMENT_MAX_BYTES);
    }

    let segments = split_server_envelope_for_transport(ServerEnvelope {
        event: ServerEvent::ServerMessage { message: Box::new(message) },
        ..witness.clone()
    }).expect("a demonstrated legal partition must not be skipped");
    assert!(segments.len() <= max_count);
    let mut reconstructed = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        assert_eq!(segment.client_id, witness.client_id);
        assert_eq!(segment.stream_id, witness.stream_id);
        assert_eq!(segment.seq_id, witness.seq_id);
        assert!(serde_json::to_vec(segment).expect("segment should serialize").len()
            <= REMOTE_CONTROL_SEGMENT_MAX_BYTES);
        let ServerEvent::ServerMessageChunk {
            segment_id, segment_count, message_size_bytes, message_chunk_base64,
        } = &segment.event else {
            panic!("oversized envelope must be segmented");
        };
        assert_eq!(*segment_id, index);
        assert_eq!(*segment_count, segments.len());
        assert_eq!(*message_size_bytes, raw.len());
        reconstructed.extend(base64::engine::general_purpose::STANDARD
            .decode(message_chunk_base64).expect("valid base64"));
    }
    assert_eq!(reconstructed, raw);
}

#[test]
fn segmentation_rejects_exhausted_count_budget() {
    let message = OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
        ConfigWarningNotification {
            summary: "x".repeat(16 * 1024),
            details: None,
            path: None,
            range: None,
        },
    ));
    let raw_size = serde_json::to_vec(&message).expect("message should serialize").len();
    let mut envelope = ServerEnvelope {
        event: ServerEvent::ServerMessageChunk {
            segment_id: 0,
            segment_count: 1,
            message_size_bytes: raw_size,
            message_chunk_base64: String::new(),
        },
        client_id: ClientId(String::new()),
        stream_id: StreamId("stream".into()),
        seq_id: 1,
    };
    // Even the smallest possible routing metadata leaves only 16 base64
    // bytes (12 raw bytes) per frame. No partition within 1024 frames can fit.
    let metadata_size = serde_json::to_vec(&envelope).unwrap().len();
    envelope.client_id = ClientId("x".repeat(REMOTE_CONTROL_SEGMENT_MAX_BYTES - metadata_size - 16));
    assert!(raw_size > 12 * super::segment::REMOTE_CONTROL_SEGMENT_COUNT_MAX);
    envelope.event = ServerEvent::ServerMessage { message: Box::new(message) };
    let error = split_server_envelope_for_transport(envelope)
        .expect_err("exhausting the finite count budget must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("segment count exceeds maximum"));
}
