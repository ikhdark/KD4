use codex_validation_contracts::canonical::ContractError;
use codex_validation_contracts::canonical::canonical_jcs;
use codex_validation_contracts::focused_evidence_frame::FOCUSED_EVIDENCE_HEADER_LEN;
use codex_validation_contracts::focused_evidence_frame::FOCUSED_EVIDENCE_MANIFEST_MAX_BYTES;
use codex_validation_contracts::focused_evidence_frame::FOCUSED_EVIDENCE_PAYLOAD_MAX_BYTES;
use codex_validation_contracts::focused_evidence_frame::FocusedEvidenceAckError;
use codex_validation_contracts::focused_evidence_frame::FocusedEvidenceAckReasonV1;
use codex_validation_contracts::focused_evidence_frame::FocusedEvidenceAckV1;
use codex_validation_contracts::focused_evidence_frame::FocusedEvidenceBodyError;
use codex_validation_contracts::focused_evidence_frame::FocusedEvidenceFrameV1;
use codex_validation_contracts::focused_evidence_frame::FocusedEvidenceHeaderError;
use codex_validation_contracts::focused_evidence_frame::FocusedEvidenceHeaderV1;
use codex_validation_contracts::focused_evidence_frame::FocusedEvidenceMemberV1;
use codex_validation_contracts::focused_live_successor::parse_inventory_discovery_process_set_canonical_v1;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

const GOLDEN_HEADER_HEX: &str = "4b443445564944310100000600000000000002c00000000000002706";
const GOLDEN_FRAME_SHA256: &str =
    "f109913301b6d6415f03438967d59161295e851ce3ae9c2fea4e6388333f9fa3";
const GOLDEN_ACCEPT_ACK_HEX: &str = concat!(
    "4b4434455641434b0100000000000000",
    "f109913301b6d6415f03438967d59161295e851ce3ae9c2fea4e6388333f9fa3"
);
const GOLDEN_MANIFEST: &[u8] = br#"{"members":[{"length":3972,"name":"catalog","sha256":"9586c37e5c8b958654dc0ddf803fa60e8e17282445c74a1fe9fea7c2278a80d1"},{"length":5960,"name":"process","sha256":"79139cf1cb06dab660b6d97647ff9e6a773969a62546d28b0d54455a5793a06a"},{"length":17,"name":"unittest_collect","sha256":"e105fda3edcb66558a8756aa859dc946bbebbfcc8cc5ee23b191733784ee85c7"},{"length":14,"name":"unittest_exec","sha256":"4fc5671eb4ae3920e9d4096de61dec1c39bba262b9fedebdbef1ab43a803737b"},{"length":15,"name":"pytest_collect","sha256":"01c660ab4f0ae79b1394976386d5421126a5a0a340ff297d6a05abcfd6143c73"},{"length":12,"name":"pytest_exec","sha256":"469eb875de3fff0963ef85da19bd9e3a8c3b148ec3b4ddd38e55fbb99fff4e5f"}],"schema_version":1}"#;

fn decode_hex(value: &str) -> Vec<u8> {
    assert_eq!(value.len() % 2, 0);
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).expect("hex fixture is ASCII");
            u8::from_str_radix(text, 16).expect("hex fixture is valid")
        })
        .collect()
}

fn raw_sha256(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn header_bytes(manifest_len: u32, payload_len: u64) -> [u8; FOCUSED_EVIDENCE_HEADER_LEN] {
    let mut header = [0_u8; FOCUSED_EVIDENCE_HEADER_LEN];
    header[..8].copy_from_slice(b"KD4EVID1");
    header[8] = 1;
    header[10..12].copy_from_slice(&6_u16.to_be_bytes());
    header[16..20].copy_from_slice(&manifest_len.to_be_bytes());
    header[20..28].copy_from_slice(&payload_len.to_be_bytes());
    header
}

fn golden_members() -> [Vec<u8>; 6] {
    let vectors: Value = serde_json::from_slice(include_bytes!(
        "../../../scripts/fixtures/focused_live_successor_catalog_v1_vectors.json"
    ))
    .expect("focused catalog vectors parse");
    let vector = &vectors["valid_vectors"][0];
    [
        vector["canonical_catalog_json"]
            .as_str()
            .expect("golden catalog JSON is a string")
            .as_bytes()
            .to_vec(),
        vector["canonical_inventory_discovery_processes_json"]
            .as_str()
            .expect("golden process JSON is a string")
            .as_bytes()
            .to_vec(),
        b"unittest-collect\n".to_vec(),
        b"unittest-exec\n".to_vec(),
        b"pytest-collect\n".to_vec(),
        b"pytest-exec\n".to_vec(),
    ]
}

fn golden_body() -> Vec<u8> {
    let mut body = GOLDEN_MANIFEST.to_vec();
    for member in golden_members() {
        body.extend_from_slice(&member);
    }
    body
}

fn manifest_value(members: &[(&str, Vec<u8>); 6]) -> Value {
    let members = members
        .iter()
        .map(|(name, bytes)| {
            json!({
                "name": name,
                "length": bytes.len(),
                "sha256": raw_sha256(bytes),
            })
        })
        .collect::<Vec<_>>();
    json!({"schema_version": 1, "members": members})
}

fn small_members() -> [(&'static str, Vec<u8>); 6] {
    [
        ("catalog", vec![b'a']),
        ("process", vec![b'b']),
        ("unittest_collect", vec![b'c']),
        ("unittest_exec", vec![b'd']),
        ("pytest_collect", vec![b'e']),
        ("pytest_exec", vec![b'f']),
    ]
}

fn frame_from_manifest(manifest: &Value, payload: &[u8]) -> (FocusedEvidenceHeaderV1, Vec<u8>) {
    let manifest = canonical_jcs(manifest).expect("test manifest canonicalizes");
    let header = header_bytes(manifest.len() as u32, payload.len() as u64);
    let header = FocusedEvidenceHeaderV1::parse(&header).expect("test header parses");
    let mut body = manifest;
    body.extend_from_slice(payload);
    (header, body)
}

fn complete_digest(error: &FocusedEvidenceBodyError) -> String {
    let digest = error
        .complete_digest()
        .expect("complete frame rejection carries its digest");
    digest
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn assert_complete_rejection(error: &FocusedEvidenceBodyError, expected_digest: &str) {
    assert_eq!(complete_digest(error), expected_digest);
    let expected_ack = decode_hex(&format!(
        "4b4434455641434b0100000100000000{expected_digest}"
    ));
    assert_eq!(
        FocusedEvidenceAckV1::rejected_complete(error.complete_digest().expect("digest"))
            .encode()
            .as_slice(),
        expected_ack.as_slice()
    );
}

#[test]
fn focused_evidence_frame_matches_cross_language_golden() {
    assert_eq!(GOLDEN_MANIFEST.len(), 704);
    let header_bytes = decode_hex(GOLDEN_HEADER_HEX);
    let header = FocusedEvidenceHeaderV1::parse(&header_bytes).expect("golden header parses");
    assert_eq!(header.manifest_len(), 704);
    assert_eq!(header.payload_len(), 9_990);
    assert_eq!(header.body_len(), 10_694);

    let members = golden_members();
    let body = golden_body();
    let frame = FocusedEvidenceFrameV1::parse_complete(header, body)
        .expect("golden focused evidence frame parses");
    assert_eq!(
        frame.digest().as_bytes().as_slice(),
        decode_hex(GOLDEN_FRAME_SHA256)
    );
    for (member, expected) in [
        FocusedEvidenceMemberV1::Catalog,
        FocusedEvidenceMemberV1::Process,
        FocusedEvidenceMemberV1::UnittestCollect,
        FocusedEvidenceMemberV1::UnittestExec,
        FocusedEvidenceMemberV1::PytestCollect,
        FocusedEvidenceMemberV1::PytestExec,
    ]
    .into_iter()
    .zip(&members)
    {
        assert_eq!(frame.member(member), expected);
    }

    let processes = parse_inventory_discovery_process_set_canonical_v1(
        frame.member(FocusedEvidenceMemberV1::Process),
    )
    .expect("golden process set is exact canonical typed evidence");
    assert_eq!(processes.len(), 6);

    let ack = FocusedEvidenceAckV1::accepted(frame.digest()).encode();
    assert_eq!(ack.as_slice(), decode_hex(GOLDEN_ACCEPT_ACK_HEX));
    assert_eq!(
        FocusedEvidenceAckV1::parse_exact(&ack, frame.digest()),
        Ok(FocusedEvidenceAckReasonV1::Accepted)
    );
}

#[test]
fn complete_invalid_frames_produce_exact_rejection_digests() {
    let header_bytes = decode_hex(GOLDEN_HEADER_HEX);

    let mut malformed = golden_body();
    malformed[0] = b'[';
    let error = FocusedEvidenceFrameV1::parse_complete(
        FocusedEvidenceHeaderV1::parse(&header_bytes).expect("golden header parses"),
        malformed,
    )
    .expect_err("malformed complete manifest is rejected");
    assert_complete_rejection(
        &error,
        "b0e90200dcac496b0e0e49ffffe2eb8b5a3577ca06738be11987128386c65635",
    );

    let mut noncanonical = golden_body();
    noncanonical.insert(1, b' ');
    let noncanonical_header =
        decode_hex("4b443445564944310100000600000000000002c10000000000002706");
    let error = FocusedEvidenceFrameV1::parse_complete(
        FocusedEvidenceHeaderV1::parse(&noncanonical_header)
            .expect("noncanonical fixture header remains frameable"),
        noncanonical,
    )
    .expect_err("noncanonical complete manifest is rejected");
    assert_complete_rejection(
        &error,
        "7e012530f56e35ad674663a06b9a8808e29815ef201bf8384506be8ab7b787bd",
    );

    let mut mismatched = golden_body();
    mismatched[GOLDEN_MANIFEST.len()] = b'z';
    let error = FocusedEvidenceFrameV1::parse_complete(
        FocusedEvidenceHeaderV1::parse(&header_bytes).expect("golden header parses"),
        mismatched,
    )
    .expect_err("member digest mismatch is rejected");
    assert_complete_rejection(
        &error,
        "22190d72b304f9615ef49f42ebbb13dbd0101a1254a56058b928a0570b7d9803",
    );
}

#[test]
fn focused_evidence_header_enforces_every_field_and_bound() {
    let valid = header_bytes(1, 0);
    assert!(FocusedEvidenceHeaderV1::parse(&valid).is_ok());
    assert_eq!(
        FocusedEvidenceHeaderV1::parse(&valid[..27]),
        Err(FocusedEvidenceHeaderError::InvalidLength { actual: 27 })
    );

    let mut changed = valid;
    changed[0] = b'X';
    assert_eq!(
        FocusedEvidenceHeaderV1::parse(&changed),
        Err(FocusedEvidenceHeaderError::InvalidMagic)
    );
    changed = valid;
    changed[8] = 2;
    assert_eq!(
        FocusedEvidenceHeaderV1::parse(&changed),
        Err(FocusedEvidenceHeaderError::UnsupportedVersion(2))
    );
    changed = valid;
    changed[9] = 1;
    assert_eq!(
        FocusedEvidenceHeaderV1::parse(&changed),
        Err(FocusedEvidenceHeaderError::InvalidFlags(1))
    );
    changed = valid;
    changed[10..12].copy_from_slice(&5_u16.to_be_bytes());
    assert_eq!(
        FocusedEvidenceHeaderV1::parse(&changed),
        Err(FocusedEvidenceHeaderError::InvalidMemberCount(5))
    );
    changed = valid;
    changed[12..16].copy_from_slice(&1_u32.to_be_bytes());
    assert_eq!(
        FocusedEvidenceHeaderV1::parse(&changed),
        Err(FocusedEvidenceHeaderError::NonzeroReserved(1))
    );

    assert_eq!(
        FocusedEvidenceHeaderV1::parse(&header_bytes(0, 0)),
        Err(FocusedEvidenceHeaderError::InvalidManifestLength(0))
    );
    assert!(
        FocusedEvidenceHeaderV1::parse(&header_bytes(
            FOCUSED_EVIDENCE_MANIFEST_MAX_BYTES as u32,
            FOCUSED_EVIDENCE_PAYLOAD_MAX_BYTES,
        ))
        .is_ok()
    );
    assert_eq!(
        FocusedEvidenceHeaderV1::parse(&header_bytes(
            FOCUSED_EVIDENCE_MANIFEST_MAX_BYTES as u32 + 1,
            0,
        )),
        Err(FocusedEvidenceHeaderError::InvalidManifestLength(16_385))
    );
    assert_eq!(
        FocusedEvidenceHeaderV1::parse(&header_bytes(1, FOCUSED_EVIDENCE_PAYLOAD_MAX_BYTES + 1,)),
        Err(FocusedEvidenceHeaderError::PayloadTooLarge(134_217_729))
    );
}

#[test]
fn focused_evidence_body_requires_exact_length() {
    let header_bytes = decode_hex(GOLDEN_HEADER_HEX);
    let mut truncated = golden_body();
    truncated.pop();
    let error = FocusedEvidenceFrameV1::parse_complete(
        FocusedEvidenceHeaderV1::parse(&header_bytes).expect("golden header parses"),
        truncated,
    )
    .expect_err("truncated frame is rejected");
    assert!(matches!(error, FocusedEvidenceBodyError::Truncated { .. }));
    assert_eq!(error.complete_digest(), None);

    let mut trailing = golden_body();
    trailing.push(0);
    let error = FocusedEvidenceFrameV1::parse_complete(
        FocusedEvidenceHeaderV1::parse(&header_bytes).expect("golden header parses"),
        trailing,
    )
    .expect_err("trailing frame byte is rejected");
    assert!(matches!(
        error,
        FocusedEvidenceBodyError::TrailingBytes { .. }
    ));
    assert_eq!(error.complete_digest(), None);
}

#[test]
fn focused_evidence_manifest_is_closed_ordered_and_exact() {
    let members = small_members();
    let payload = members
        .iter()
        .flat_map(|(_, bytes)| bytes.iter().copied())
        .collect::<Vec<_>>();
    let valid = manifest_value(&members);
    let (header, body) = frame_from_manifest(&valid, &payload);
    FocusedEvidenceFrameV1::parse_complete(header, body).expect("small exact frame parses");

    let mut cases = Vec::new();
    let mut wrong_schema = valid.clone();
    wrong_schema["schema_version"] = json!(2);
    cases.push(wrong_schema);
    let mut unknown_root = valid.clone();
    unknown_root["unexpected"] = Value::Null;
    cases.push(unknown_root);
    let mut wrong_count = valid.clone();
    wrong_count["members"]
        .as_array_mut()
        .expect("members array")
        .pop();
    cases.push(wrong_count);
    let mut unknown_member = valid.clone();
    unknown_member["members"][0]["unexpected"] = Value::Null;
    cases.push(unknown_member);
    let mut wrong_order = valid.clone();
    let ordered = wrong_order["members"]
        .as_array_mut()
        .expect("members array");
    ordered.swap(0, 1);
    cases.push(wrong_order);
    let mut wrong_name = valid.clone();
    wrong_name["members"][0]["name"] = json!("other");
    cases.push(wrong_name);
    let mut zero_length = valid.clone();
    zero_length["members"][0]["length"] = json!(0);
    zero_length["members"][1]["length"] = json!(2);
    cases.push(zero_length);
    let mut uppercase_sha = valid.clone();
    uppercase_sha["members"][0]["sha256"] = json!(raw_sha256(&members[0].1).to_ascii_uppercase());
    cases.push(uppercase_sha);
    let mut digest_mismatch = valid.clone();
    digest_mismatch["members"][0]["sha256"] = json!("0".repeat(64));
    cases.push(digest_mismatch);

    for manifest in cases {
        let (header, body) = frame_from_manifest(&manifest, &payload);
        let error = FocusedEvidenceFrameV1::parse_complete(header, body)
            .expect_err("invalid complete manifest is rejected");
        assert!(error.complete_digest().is_some());
    }

    let mut sum_mismatch = valid.clone();
    sum_mismatch["members"][0]["length"] = json!(2);
    let (header, body) = frame_from_manifest(&sum_mismatch, &payload);
    assert!(matches!(
        FocusedEvidenceFrameV1::parse_complete(header, body),
        Err(FocusedEvidenceBodyError::CompleteRejected {
            error: ContractError::InvalidContract(_),
            ..
        })
    ));

    let mut padded_payload = payload.clone();
    padded_payload.push(b'g');
    let (header, body) = frame_from_manifest(&valid, &padded_payload);
    assert!(matches!(
        FocusedEvidenceFrameV1::parse_complete(header, body),
        Err(FocusedEvidenceBodyError::CompleteRejected { .. })
    ));
}

#[test]
fn focused_evidence_ack_is_closed_and_digest_bound() {
    let header_bytes = decode_hex(GOLDEN_HEADER_HEX);
    let frame = FocusedEvidenceFrameV1::parse_complete(
        FocusedEvidenceHeaderV1::parse(&header_bytes).expect("golden header parses"),
        golden_body(),
    )
    .expect("golden frame parses");
    let accepted = FocusedEvidenceAckV1::accepted(frame.digest()).encode();
    let rejected = FocusedEvidenceAckV1::rejected_complete(frame.digest()).encode();
    assert_eq!(
        FocusedEvidenceAckV1::parse_exact(&rejected, frame.digest()),
        Ok(FocusedEvidenceAckReasonV1::Rejected)
    );

    assert_eq!(
        FocusedEvidenceAckV1::parse_exact(&accepted[..47], frame.digest()),
        Err(FocusedEvidenceAckError::InvalidLength { actual: 47 })
    );
    let mut overlong = accepted.to_vec();
    overlong.push(0);
    assert_eq!(
        FocusedEvidenceAckV1::parse_exact(&overlong, frame.digest()),
        Err(FocusedEvidenceAckError::InvalidLength { actual: 49 })
    );

    let mut changed = accepted;
    changed[0] = b'X';
    assert_eq!(
        FocusedEvidenceAckV1::parse_exact(&changed, frame.digest()),
        Err(FocusedEvidenceAckError::InvalidMagic)
    );
    changed = accepted;
    changed[8] = 2;
    assert_eq!(
        FocusedEvidenceAckV1::parse_exact(&changed, frame.digest()),
        Err(FocusedEvidenceAckError::UnsupportedVersion(2))
    );
    changed = accepted;
    changed[9] = 1;
    assert_eq!(
        FocusedEvidenceAckV1::parse_exact(&changed, frame.digest()),
        Err(FocusedEvidenceAckError::InvalidFlags(1))
    );
    changed = accepted;
    changed[12..16].copy_from_slice(&1_u32.to_be_bytes());
    assert_eq!(
        FocusedEvidenceAckV1::parse_exact(&changed, frame.digest()),
        Err(FocusedEvidenceAckError::NonzeroReserved(1))
    );
    for reason in [2_u16, 6, u16::MAX] {
        changed = accepted;
        changed[10..12].copy_from_slice(&reason.to_be_bytes());
        assert_eq!(
            FocusedEvidenceAckV1::parse_exact(&changed, frame.digest()),
            Err(FocusedEvidenceAckError::ReservedReason(reason))
        );
    }
    changed = accepted;
    changed[47] ^= 1;
    assert_eq!(
        FocusedEvidenceAckV1::parse_exact(&changed, frame.digest()),
        Err(FocusedEvidenceAckError::DigestMismatch)
    );
}

#[test]
fn process_set_atomic_parser_rejects_noncanonical_and_unknown_fields() {
    let process_bytes = golden_members()[1].clone();
    let value: Value = serde_json::from_slice(&process_bytes).expect("process JSON parses");
    let pretty = serde_json::to_vec_pretty(&value).expect("process JSON pretty-serializes");
    assert_eq!(
        parse_inventory_discovery_process_set_canonical_v1(&pretty),
        Err(ContractError::NonCanonicalJson)
    );

    let mut unknown = value;
    unknown[0]["unexpected"] = Value::Null;
    let unknown = canonical_jcs(&unknown).expect("unknown-field process JSON canonicalizes");
    assert!(matches!(
        parse_inventory_discovery_process_set_canonical_v1(&unknown),
        Err(ContractError::InvalidJson(_))
    ));
}
