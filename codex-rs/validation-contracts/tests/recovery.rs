use codex_validation_contracts::canonical::canonical_jcs;
use codex_validation_contracts::canonical::proof_hash;
use codex_validation_contracts::recovery::CanonicalParameterProjectionV1;
use codex_validation_contracts::recovery::UnittestRecapturePacketV1;
use codex_validation_contracts::recovery::UnittestSourceProvenanceExceptionV1;
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;

fn recapture() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.codex/validation/frozen-test-inventory-v2-unittest-recapture.json");
    serde_json::from_slice(&std::fs::read(path).expect("materialized gate-9 recapture"))
        .expect("recapture JSON")
}

fn validate(value: Value) -> Result<(), String> {
    let packet: UnittestRecapturePacketV1 =
        serde_json::from_value(value).map_err(|error| error.to_string())?;
    packet.validate().map_err(|error| error.to_string())
}

fn rehash(value: &mut Value) {
    value
        .as_object_mut()
        .expect("packet object")
        .remove("receipt_sha256");
    value["receipt_sha256"] = json!(
        proof_hash(UnittestRecapturePacketV1::RECEIPT_HASH_DOMAIN, value).expect("packet hashes")
    );
}

#[test]
fn approved_recapture_keeps_frozen_identities_and_only_observed_children() {
    let packet: UnittestRecapturePacketV1 =
        serde_json::from_value(recapture()).expect("typed packet");
    packet.validate().expect("real recapture validates");
    assert_eq!(packet.parent_records.len(), 893);
    assert_eq!(packet.parent_results.len(), 859);
    assert_eq!(packet.subtest_manifests().expect("manifests").len(), 859);
    let approval = packet
        .source_provenance_exception
        .as_ref()
        .expect("approval");
    let excepted: std::collections::BTreeSet<_> = approval
        .baseline_ids
        .iter()
        .chain(&approval.historical_execution_extension.baseline_ids)
        .collect();
    assert_eq!(excepted.len(), 34);
    let children = packet.recovered_child_sources().expect("observed children");
    assert_eq!(children.len(), 1228);
    for child in children {
        assert!(!excepted.contains(&child.parent_baseline_id));
        assert!(
            !child
                .parent_baseline_id
                .starts_with("hidden-at-freeze-v1::")
        );
    }
}

#[test]
fn rehashed_packets_cannot_expand_approval_or_claim_excepted_execution() {
    let original = recapture();
    let mut no_approval = original.clone();
    no_approval
        .as_object_mut()
        .expect("object")
        .remove("source_provenance_exception");
    rehash(&mut no_approval);
    assert!(
        validate(no_approval)
            .expect_err("missing approval")
            .contains("source authority is unavailable")
    );
    let mut expanded = original.clone();
    expanded["source_provenance_exception"]["historical_execution_extension"]["baseline_ids"]
        .as_array_mut()
        .expect("IDs")
        .push(json!("extra-parent"));
    rehash(&mut expanded);
    assert!(
        validate(expanded)
            .expect_err("expanded approval")
            .contains("exact approved five-source and 29-parent amendments")
    );
    for excepted in [
        &original["source_provenance_exception"]["baseline_ids"][0],
        &original["source_provenance_exception"]["historical_execution_extension"]["baseline_ids"]
            [0],
    ] {
        let mut forged = original.clone();
        forged["parent_results"][0]["parent_baseline_id"] = excepted.clone();
        rehash(&mut forged);
        assert!(
            validate(forged).is_err(),
            "excepted parent cannot become execution evidence"
        );
    }
    let mut changed_sites = original;
    changed_sites["source_site_manifest"][0]["line"] = json!(1);
    rehash(&mut changed_sites);
    assert!(
        validate(changed_sites).is_err(),
        "rehashing cannot authenticate different source"
    );
}

#[test]
fn finite_float_parameters_preserve_bits_and_reject_noncanonical_values() {
    for bits in ["0000000000000000", "8000000000000000", "3ff8000000000000"] {
        let value = json!({"kind": "float64", "bits": bits});
        let projection: CanonicalParameterProjectionV1 =
            serde_json::from_value(value.clone()).expect("typed float parameter");
        projection.validate().expect("finite binary64 parameter");
        assert_eq!(
            serde_json::to_value(projection).expect("serialization"),
            value
        );
        canonical_jcs(&value).expect("float bits require no floating JSON number");
    }
    for bits in [
        "7ff0000000000000",
        "7ff8000000000000",
        "3FF8000000000000",
        "0",
    ] {
        let projection: CanonicalParameterProjectionV1 =
            serde_json::from_value(json!({"kind": "float64", "bits": bits})).expect("shape");
        assert!(projection.validate().is_err(), "invalid bits: {bits}");
    }
}

#[test]
fn source_exception_requires_the_exact_approved_five_and_29_parent_authorities() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.codex/validation/frozen-test-inventory-v2-unittest-source-exceptions.json");
    let raw = std::fs::read(path).expect("approved source exception");
    let approved: Value = serde_json::from_slice(&raw).expect("JSON");
    let exception: UnittestSourceProvenanceExceptionV1 =
        serde_json::from_value(approved.clone()).expect("typed exception");
    exception.validate().expect("exact current-user authority");
    assert_eq!(canonical_jcs(&approved).expect("canonical approval"), raw);
    assert_eq!(exception.baseline_ids.len(), 5);
    assert_eq!(
        exception.historical_execution_extension.baseline_ids.len(),
        29
    );
    let mut extended = approved.clone();
    extended["historical_execution_extension"]["baseline_ids"]
        .as_array_mut()
        .unwrap()
        .push(json!("unapproved-30th-parent"));
    assert!(
        serde_json::from_value::<UnittestSourceProvenanceExceptionV1>(extended)
            .unwrap()
            .validate()
            .is_err()
    );
    for field in ["baseline_ids", "candidate_git_blobs", "requirements"] {
        let mut tampered = approved.clone();
        tampered[field]
            .as_array_mut()
            .expect("array")
            .push(json!("unapproved-addition"));
        let exception: UnittestSourceProvenanceExceptionV1 =
            serde_json::from_value(tampered).expect("same typed shape");
        assert!(
            exception.validate().is_err(),
            "unapproved change to {field}"
        );
    }
    let mut tampered = approved;
    tampered["authority"]["source_task_id"] = json!("different-task");
    let exception: UnittestSourceProvenanceExceptionV1 =
        serde_json::from_value(tampered).expect("same typed shape");
    assert!(
        exception.validate().is_err(),
        "approval cannot be attributed elsewhere"
    );
}
