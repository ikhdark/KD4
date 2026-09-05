use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_validation_contracts::canonical::ContractError;
use codex_validation_contracts::canonical::Sha256HexV1;
use codex_validation_contracts::canonical::canonical_jcs;
use codex_validation_contracts::canonical::parse_canonical_jcs;
use codex_validation_contracts::focused_replacement_approval::FocusedReplacementApprovalCurrentContextV1;
use codex_validation_contracts::focused_replacement_approval::FocusedReplacementApprovalReceiptV1;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Vectors {
    format_id: String,
    schema_version: u32,
    receipt_format_id: String,
    hash_domain: String,
    digest_rule: String,
    valid_vectors: Vec<ValidVector>,
    invalid_vectors: Vec<InvalidVector>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidVector {
    case: String,
    receipt: FocusedReplacementApprovalReceiptV1,
    #[serde(default)]
    canonical_digest_input: Option<String>,
    expected_receipt_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InvalidVector {
    case: String,
    kind: String,
    #[serde(default)]
    raw_json_base64url: Option<String>,
    #[serde(default)]
    semantic_receipt_sha256: Option<String>,
    #[serde(default)]
    receipt: Option<Value>,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    trusted_current_context: Option<TrustedCurrentContextVectorValue>,
    expected: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedCurrentContextVectorValue {
    format_id: String,
    schema_version: u32,
    attempt_id: String,
    focused_validation_id: String,
    classification: String,
    frozen_inventory_hash: Sha256HexV1,
    focused_inventory_catalog_semantic_sha256: Sha256HexV1,
    inventory_discovery_processes_sha256: Sha256HexV1,
    policy_id: String,
    policy_runner_bundle_sha256: Sha256HexV1,
    workspace_fingerprint: Sha256HexV1,
    mutation_epoch: u64,
}

impl From<TrustedCurrentContextVectorValue> for FocusedReplacementApprovalCurrentContextV1 {
    fn from(value: TrustedCurrentContextVectorValue) -> Self {
        Self {
            format_id: value.format_id,
            schema_version: value.schema_version,
            attempt_id: value.attempt_id,
            focused_validation_id: value.focused_validation_id,
            classification: value.classification,
            frozen_inventory_hash: value.frozen_inventory_hash,
            focused_inventory_catalog_semantic_sha256: value
                .focused_inventory_catalog_semantic_sha256,
            inventory_discovery_processes_sha256: value.inventory_discovery_processes_sha256,
            policy_id: value.policy_id,
            policy_runner_bundle_sha256: value.policy_runner_bundle_sha256,
            workspace_fingerprint: value.workspace_fingerprint,
            mutation_epoch: value.mutation_epoch,
        }
    }
}

fn current_context(
    receipt: &FocusedReplacementApprovalReceiptV1,
) -> FocusedReplacementApprovalCurrentContextV1 {
    FocusedReplacementApprovalCurrentContextV1 {
        format_id: receipt.format_id.clone(),
        schema_version: receipt.schema_version,
        attempt_id: receipt.attempt_id.clone(),
        focused_validation_id: receipt.focused_validation_id.clone(),
        classification: receipt.classification.clone(),
        frozen_inventory_hash: receipt.frozen_inventory_hash.clone(),
        focused_inventory_catalog_semantic_sha256: receipt
            .focused_inventory_catalog_semantic_sha256
            .clone(),
        inventory_discovery_processes_sha256: receipt.inventory_discovery_processes_sha256.clone(),
        policy_id: receipt.policy_id.clone(),
        policy_runner_bundle_sha256: receipt.policy_runner_bundle_sha256.clone(),
        workspace_fingerprint: receipt.workspace_fingerprint.clone(),
        mutation_epoch: receipt.mutation_epoch,
    }
}

#[test]
fn focused_replacement_approval_receipt_vectors_match_python() {
    let vectors: Vectors = serde_json::from_slice(include_bytes!(
        "../../../scripts/fixtures/focused_replacement_approval_receipt_v1_vectors.json"
    ))
    .expect("approval vectors parse");
    assert_eq!(
        vectors.format_id,
        "kd4.focused-replacement-approval-receipt.v1.test-vectors"
    );
    assert_eq!(vectors.schema_version, 1);
    assert_eq!(
        vectors.receipt_format_id,
        FocusedReplacementApprovalReceiptV1::FORMAT_ID
    );
    assert_eq!(
        vectors.hash_domain,
        FocusedReplacementApprovalReceiptV1::HASH_DOMAIN
    );
    assert!(vectors.digest_rule.contains("strict JCS"));

    for vector in vectors.valid_vectors {
        let trusted = current_context(&vector.receipt);
        vector
            .receipt
            .validate()
            .unwrap_or_else(|error| panic!("{} must validate: {error}", vector.case));
        vector
            .receipt
            .validate_current_context(&trusted)
            .unwrap_or_else(|error| panic!("{} must match current context: {error}", vector.case));
        assert_eq!(
            vector.receipt.receipt_sha256.as_str(),
            vector.expected_receipt_sha256,
            "{}",
            vector.case
        );
        assert_eq!(
            vector
                .receipt
                .receipt_sha256()
                .expect("receipt digest computes")
                .as_str(),
            vector.expected_receipt_sha256,
            "{}",
            vector.case
        );
        if let Some(expected) = vector.canonical_digest_input {
            let mut projection = serde_json::to_value(&vector.receipt).expect("receipt serializes");
            projection
                .as_object_mut()
                .expect("receipt is an object")
                .remove("receipt_sha256");
            assert_eq!(
                canonical_jcs(&projection).expect("projection canonicalizes"),
                expected.as_bytes(),
                "{}",
                vector.case
            );
        }
    }

    for vector in vectors.invalid_vectors {
        assert_eq!(vector.expected, "reject", "{}", vector.case);
        assert!(!vector.reason.is_empty(), "{}", vector.case);
        match vector.kind.as_str() {
            "raw-json-bytes" => {
                let raw = URL_SAFE_NO_PAD
                    .decode(
                        vector
                            .raw_json_base64url
                            .as_deref()
                            .expect("raw vector carries bytes"),
                    )
                    .expect("raw vector is base64url");
                assert!(parse_canonical_jcs(&raw).is_err(), "{}", vector.case);
                assert!(vector.semantic_receipt_sha256.is_some(), "{}", vector.case);
            }
            "receipt" => {
                let rejected = match serde_json::from_value::<FocusedReplacementApprovalReceiptV1>(
                    vector.receipt.expect("receipt vector carries a receipt"),
                ) {
                    Ok(receipt) => receipt.validate().is_err(),
                    Err(_) => true,
                };
                assert!(rejected, "{}", vector.case);
            }
            "value" => {
                assert!(
                    serde_json::from_value::<FocusedReplacementApprovalReceiptV1>(
                        vector.value.expect("value vector carries a value")
                    )
                    .is_err(),
                    "{}",
                    vector.case
                );
            }
            "trusted-current-context" => {
                let receipt = serde_json::from_value::<FocusedReplacementApprovalReceiptV1>(
                    vector
                        .receipt
                        .expect("trusted current context vector carries a receipt"),
                )
                .expect("trusted current context vector receipt is intrinsically valid");
                let trusted = vector
                    .trusted_current_context
                    .expect("trusted current context vector carries trusted context")
                    .into();
                assert!(
                    receipt.validate_current_context(&trusted).is_err(),
                    "{}",
                    vector.case
                );
            }
            other => panic!("unknown invalid vector kind {other:?}"),
        }
    }
}

#[test]
fn focused_approval_atomic_canonical_parser_closes_the_typed_contract() {
    let vectors: Vectors = serde_json::from_slice(include_bytes!(
        "../../../scripts/fixtures/focused_replacement_approval_receipt_v1_vectors.json"
    ))
    .expect("approval vectors parse");
    let receipt = &vectors.valid_vectors[0].receipt;
    let canonical =
        canonical_jcs(&serde_json::to_value(receipt).expect("focused approval receipt serializes"))
            .expect("focused approval receipt canonicalizes");
    assert_eq!(
        FocusedReplacementApprovalReceiptV1::parse_canonical(&canonical)
            .expect("valid canonical focused approval receipt parses"),
        receipt.clone()
    );

    let pretty =
        serde_json::to_vec_pretty(receipt).expect("focused approval receipt pretty-serializes");
    assert_eq!(
        FocusedReplacementApprovalReceiptV1::parse_canonical(&pretty),
        Err(ContractError::NonCanonicalJson)
    );

    let mut unknown = serde_json::to_value(receipt).expect("focused approval receipt serializes");
    unknown
        .as_object_mut()
        .expect("focused approval receipt is an object")
        .insert("unexpected".to_owned(), Value::Bool(true));
    let unknown = canonical_jcs(&unknown).expect("unknown-field approval receipt canonicalizes");
    assert!(matches!(
        FocusedReplacementApprovalReceiptV1::parse_canonical(&unknown),
        Err(ContractError::InvalidJson(_))
    ));

    let mut invalid = receipt.clone();
    invalid.receipt_sha256 = Sha256HexV1::parse("0".repeat(64)).expect("test digest parses");
    let invalid = canonical_jcs(
        &serde_json::to_value(&invalid).expect("invalid focused approval receipt serializes"),
    )
    .expect("invalid focused approval receipt canonicalizes");
    assert!(matches!(
        FocusedReplacementApprovalReceiptV1::parse_canonical(&invalid),
        Err(ContractError::InvalidContract(_))
    ));
}

#[test]
fn a_self_consistent_receipt_cannot_forge_the_trusted_current_context() {
    let vectors: Vectors = serde_json::from_slice(include_bytes!(
        "../../../scripts/fixtures/focused_replacement_approval_receipt_v1_vectors.json"
    ))
    .expect("approval vectors parse");
    let mut receipt = vectors
        .valid_vectors
        .into_iter()
        .next()
        .expect("approval fixture has a valid vector")
        .receipt;
    let trusted = current_context(&receipt);

    receipt.workspace_fingerprint =
        Sha256HexV1::parse("0".repeat(64)).expect("forged fingerprint is a valid digest");
    receipt.receipt_sha256 = receipt
        .receipt_sha256()
        .expect("forged receipt hash computes");

    receipt
        .validate()
        .expect("a recomputed self-hash proves intrinsic integrity only");
    assert!(receipt.validate_current_context(&trusted).is_err());
}

#[test]
fn focused_replacement_approval_receipts_match_the_closed_schema() {
    let vectors: Value = serde_json::from_slice(include_bytes!(
        "../../../scripts/fixtures/focused_replacement_approval_receipt_v1_vectors.json"
    ))
    .expect("approval vectors parse");
    let schema: Value = serde_json::from_slice(include_bytes!(
        "../../../.codex/validation/focused-replacement-approval-receipt-v1.schema.json"
    ))
    .expect("approval schema parses");
    let validator = jsonschema::validator_for(&schema).expect("approval schema compiles");
    for vector in vectors["valid_vectors"]
        .as_array()
        .expect("valid vectors are an array")
    {
        validator
            .validate(&vector["receipt"])
            .unwrap_or_else(|error| panic!("schema rejected {}: {error}", vector["case"]));
    }
}
