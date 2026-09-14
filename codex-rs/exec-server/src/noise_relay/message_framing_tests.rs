use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::JSONRPCNotification;
use pretty_assertions::assert_eq;

use super::JsonRpcMessageDecoder;
use super::MAX_NOISE_JSONRPC_MESSAGE_LEN;
use super::NOISE_RECORD_PLAINTEXT_LEN;
use super::frame_jsonrpc_message;
use crate::ExecServerError;

#[test]
fn serialization_accepts_exact_limit_and_rejects_next_byte() {
    let message = JSONRPCMessage::Notification(JSONRPCNotification {
        method: "test".to_string(),
        params: None,
    });
    let expected = frame_jsonrpc_message(&message).unwrap();
    let payload_len = expected.len() - super::LENGTH_PREFIX_BYTES;
    assert_eq!(
        super::frame_jsonrpc_message_with_limit(&message, payload_len).unwrap(),
        expected
    );
    assert!(matches!(
        super::frame_jsonrpc_message_with_limit(&message, payload_len - 1),
        Err(ExecServerError::Protocol(message))
            if message == "Noise relay JSON-RPC message exceeds maximum length"
    ));

    let mut framed = Vec::new();
    let mut writer = super::BoundedMessageWriter {
        framed: &mut framed,
        remaining: 3,
        exceeded: false,
    };
    std::io::Write::write_all(&mut writer, b"abc").unwrap();
    assert!(std::io::Write::write_all(&mut writer, b"d").is_err());
    assert_eq!(framed, b"abc");
}

#[test]
fn split_prefix_coalesced_messages_and_incomplete_tail() {
    let messages: Vec<_> = ["first", "second", "third"]
        .into_iter()
        .map(|method| {
            JSONRPCMessage::Notification(JSONRPCNotification {
                method: method.to_string(),
                params: None,
            })
        })
        .collect();
    let framed: Vec<_> = messages
        .iter()
        .map(|message| frame_jsonrpc_message(message).unwrap())
        .collect();
    let mut decoder = JsonRpcMessageDecoder::default();
    assert!(decoder.push(&framed[0][..2]).unwrap().is_empty());
    let mut remainder = framed[0][2..].to_vec();
    remainder.extend_from_slice(&framed[1]);
    remainder.extend_from_slice(&framed[2][..5]);
    assert_eq!(decoder.push(&remainder).unwrap(), messages[..2]);
    assert_eq!(decoder.push(&framed[2][5..]).unwrap(), messages[2..]);
    assert!(decoder.buffered.is_empty());
    assert!(decoder.buffered.capacity() > 0);
}

#[test]
fn releases_large_reassembly_capacity_after_delivery() {
    let message = JSONRPCMessage::Notification(JSONRPCNotification {
        method: "large".to_string(),
        params: Some(serde_json::json!(
            "x".repeat(super::MAX_RETAINED_BUFFER_CAPACITY)
        )),
    });
    let framed = frame_jsonrpc_message(&message).unwrap();
    let mut decoder = JsonRpcMessageDecoder::default();
    let mut decoded = Vec::new();
    for record in framed.chunks(NOISE_RECORD_PLAINTEXT_LEN) {
        decoded.extend(decoder.push(record).unwrap());
    }
    assert_eq!(decoded, vec![message]);
    assert_eq!(decoder.buffered.capacity(), 0);
}

#[test]
fn fragments_and_reassembles_large_jsonrpc_message() {
    let message = JSONRPCMessage::Notification(JSONRPCNotification {
        method: "large/test".to_string(),
        params: Some(serde_json::json!({
            "data": "x".repeat(128 * 1024),
        })),
    });
    let framed = frame_jsonrpc_message(&message).unwrap();
    assert!(framed.len() > 128 * 1024);

    let mut decoder = JsonRpcMessageDecoder::default();
    let mut decoded = Vec::new();
    for record in framed.chunks(NOISE_RECORD_PLAINTEXT_LEN) {
        decoded.extend(decoder.push(record).unwrap());
    }

    assert_eq!(decoded, vec![message]);
}

#[test]
fn rejects_declared_message_length_above_limit_without_payload() {
    let mut decoder = JsonRpcMessageDecoder::default();
    let declared_len = (MAX_NOISE_JSONRPC_MESSAGE_LEN as u32 + 1).to_be_bytes();

    assert!(matches!(
        decoder.push(&declared_len),
        Err(ExecServerError::Protocol(message))
            if message == "Noise relay JSON-RPC message has invalid length"
    ));
}

#[test]
fn rejects_oversized_plaintext_record() {
    let mut decoder = JsonRpcMessageDecoder::default();

    assert!(matches!(
        decoder.push(&vec![0; NOISE_RECORD_PLAINTEXT_LEN + 1]),
        Err(ExecServerError::Protocol(message))
            if message == "Noise relay plaintext record exceeds maximum length"
    ));
}
