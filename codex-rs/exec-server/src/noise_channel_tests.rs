use pretty_assertions::assert_eq;

use super::InitiatorHandshake;
use super::MAX_MESSAGE_LEN;
use super::NOISE_CHANNEL_SUITE;
use super::NoiseChannelError;
use super::NoiseChannelIdentity;
use super::NoiseChannelPublicKey;
use super::PendingResponderHandshake;
use super::noise_channel_prologue;

#[test]
fn hybrid_ik_roundtrip_authenticates_both_endpoints() {
    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    let responder = NoiseChannelIdentity::generate().expect("generate responder identity");
    let prologue = noise_channel_prologue("env-1", "registration-1", "stream-1");
    let authorization = b"harness-key-authorization";

    let (initiator_handshake, request) = InitiatorHandshake::start(
        &initiator,
        &responder.public_key(),
        &prologue,
        authorization,
    )
    .expect("start initiator handshake");
    let responder_handshake =
        PendingResponderHandshake::read_request(&responder, &prologue, &request)
            .expect("read responder handshake");

    assert_eq!(
        &responder_handshake.initiator_public_key,
        &initiator.public_key()
    );
    assert_eq!(responder_handshake.payload.as_slice(), authorization);

    let (mut responder_transport, response) = responder_handshake
        .complete()
        .expect("complete responder handshake");
    let mut initiator_transport = initiator_handshake
        .finish(&response)
        .expect("complete initiator handshake");

    let request_ciphertext = initiator_transport
        .encrypt(b"request")
        .expect("encrypt request");
    assert_ne!(request_ciphertext, b"request");
    assert_eq!(
        responder_transport
            .decrypt(&request_ciphertext)
            .expect("decrypt request"),
        b"request"
    );

    let response_ciphertext = responder_transport
        .encrypt(b"response")
        .expect("encrypt response");
    assert_ne!(response_ciphertext, b"response");
    assert_eq!(
        initiator_transport
            .decrypt(&response_ciphertext)
            .expect("decrypt response"),
        b"response"
    );
}

#[test_case::test_case(true; "wrong_dh")]
#[test_case::test_case(false; "wrong_kem")]
fn initiator_rejects_wrong_responder_key(wrong_dh: bool) {
    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    let expected_responder = NoiseChannelIdentity::generate().expect("generate expected identity");
    let actual_responder = NoiseChannelIdentity::generate().expect("generate actual identity");
    let mut expected_key = actual_responder.public_key();
    if wrong_dh {
        expected_key.x25519_public_key = expected_responder.public_key().x25519_public_key;
    } else {
        expected_key.mlkem768_public_key = expected_responder.public_key().mlkem768_public_key;
    }
    let prologue = noise_channel_prologue("env-1", "registration-1", "stream-1");

    let (_initiator_handshake, request) =
        InitiatorHandshake::start(&initiator, &expected_key, &prologue, b"authorization")
            .expect("start initiator handshake");

    assert!(
        PendingResponderHandshake::read_request(&actual_responder, &prologue, &request).is_err()
    );
}

#[test]
fn responder_rejects_mismatched_prologue() {
    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    let responder = NoiseChannelIdentity::generate().expect("generate responder identity");
    let initiator_prologue = noise_channel_prologue("env-1", "registration-1", "stream-1");
    let responder_prologue = noise_channel_prologue("env-1", "registration-1", "stream-2");
    let (_initiator_handshake, request) = InitiatorHandshake::start(
        &initiator,
        &responder.public_key(),
        &initiator_prologue,
        b"authorization",
    )
    .expect("start initiator handshake");

    assert!(
        PendingResponderHandshake::read_request(&responder, &responder_prologue, &request).is_err()
    );
}

#[test]
fn prologue_encoding_is_stable_and_unambiguous() {
    let prologue = noise_channel_prologue("env-1", "registration-1", "stream-1");

    assert_eq!(
        prologue,
        b"\x00\x00\x00\x00\x00\x00\x00\x20codex-exec-server-relay-noise/v1\
          \x00\x00\x00\x00\x00\x00\x00\x05env-1\
          \x00\x00\x00\x00\x00\x00\x00\x0eregistration-1\
          \x00\x00\x00\x00\x00\x00\x00\x08stream-1"
            .to_vec()
    );
}

#[test]
fn transport_rejects_tampered_ciphertext() {
    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    let responder = NoiseChannelIdentity::generate().expect("generate responder identity");
    let prologue = noise_channel_prologue("env-1", "registration-1", "stream-1");
    let (initiator_handshake, request) = InitiatorHandshake::start(
        &initiator,
        &responder.public_key(),
        &prologue,
        b"authorization",
    )
    .expect("start initiator handshake");
    let responder_handshake =
        PendingResponderHandshake::read_request(&responder, &prologue, &request)
            .expect("read responder handshake");
    let (mut responder_transport, response) = responder_handshake
        .complete()
        .expect("complete responder handshake");
    let mut initiator_transport = initiator_handshake
        .finish(&response)
        .expect("complete initiator handshake");
    let mut ciphertext = initiator_transport
        .encrypt(b"request")
        .expect("encrypt request");
    ciphertext[0] ^= 1;

    assert!(responder_transport.decrypt(&ciphertext).is_err());
}

#[test]
fn transport_rejects_replayed_ciphertext() {
    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    let responder = NoiseChannelIdentity::generate().expect("generate responder identity");
    let prologue = noise_channel_prologue("env-1", "registration-1", "stream-1");
    let (initiator_handshake, request) = InitiatorHandshake::start(
        &initiator,
        &responder.public_key(),
        &prologue,
        b"authorization",
    )
    .expect("start initiator handshake");
    let responder_handshake =
        PendingResponderHandshake::read_request(&responder, &prologue, &request)
            .expect("read responder handshake");
    let (mut responder_transport, response) = responder_handshake
        .complete()
        .expect("complete responder handshake");
    let mut initiator_transport = initiator_handshake
        .finish(&response)
        .expect("complete initiator handshake");
    let ciphertext = initiator_transport
        .encrypt(b"request")
        .expect("encrypt request");

    assert_eq!(
        responder_transport
            .decrypt(&ciphertext)
            .expect("decrypt request"),
        b"request"
    );
    assert!(matches!(
        responder_transport.decrypt(&ciphertext),
        Err(NoiseChannelError::Transport(_))
    ));
}

#[test]
fn public_key_validation_rejects_unknown_suite() {
    let key = NoiseChannelIdentity::generate()
        .expect("generate identity")
        .public_key();
    let json = serde_json::to_value(key).expect("serialize key");
    let mut object = json.as_object().expect("key object").clone();
    object.insert("suite".to_string(), serde_json::json!("unknown"));
    let key: NoiseChannelPublicKey =
        serde_json::from_value(serde_json::Value::Object(object)).expect("deserialize key");

    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    assert!(InitiatorHandshake::start(&initiator, &key, b"prologue", b"").is_err());
}

#[test]
fn public_key_serializes_with_expected_suite() {
    let key = NoiseChannelIdentity::generate()
        .expect("generate identity")
        .public_key();

    let json = serde_json::to_value(key).expect("serialize key");

    assert_eq!(json["suite"], NOISE_CHANNEL_SUITE);
}

#[test_case::test_case(true; "dh")]
#[test_case::test_case(false; "kem")]
fn handshake_rejects_oversized_encoded_keys_before_decoding(dh: bool) {
    let initiator = NoiseChannelIdentity::generate().expect("initiator");
    let mut key = NoiseChannelIdentity::generate()
        .expect("responder")
        .public_key();
    let expected = if dh {
        key.x25519_public_key = "!".repeat(4096);
        "invalid X25519 public key length"
    } else {
        key.mlkem768_public_key = "!".repeat(4096);
        "invalid ML-KEM-768 public key length"
    };
    assert!(matches!(
        InitiatorHandshake::start(&initiator, &key, b"prologue", b""),
        Err(NoiseChannelError::InvalidPublicKey(message)) if message == expected
    ));
}

#[test]
fn initiator_rejects_oversized_handshake_payload() {
    let initiator = NoiseChannelIdentity::generate().expect("generate initiator identity");
    let responder = NoiseChannelIdentity::generate().expect("generate responder identity");
    let payload = vec![0; MAX_MESSAGE_LEN];

    let result =
        InitiatorHandshake::start(&initiator, &responder.public_key(), b"prologue", &payload);

    assert!(matches!(
        result,
        Err(NoiseChannelError::InvalidMessage(
            "handshake payload is too large"
        ))
    ));
}
