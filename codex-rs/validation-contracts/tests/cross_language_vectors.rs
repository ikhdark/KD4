use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_validation_contracts::applicability::ActiveHostApplicabilityAuthorityV1;
use codex_validation_contracts::applicability::ActiveHostApplicabilityIssuerV1;
use codex_validation_contracts::applicability::ApplicabilityResultV1;
use codex_validation_contracts::applicability::CargoBuildContextObservationV1;
use codex_validation_contracts::applicability::HostTokenV1;
use codex_validation_contracts::applicability::PlatformApplicabilityV1;
use codex_validation_contracts::applicability::RustCfgExpressionV1;
use codex_validation_contracts::canonical::Sha256HexV1;
use codex_validation_contracts::canonical::canonical_jcs;
use codex_validation_contracts::canonical::proof_hash;
use codex_validation_contracts::inventory_v2::CargoTargetContextSpecV1;
use codex_validation_contracts::inventory_v2::ExecutableInventoryEntryV2;
use codex_validation_contracts::inventory_v2::FrozenBaselineAssociationV1;
use codex_validation_contracts::inventory_v2::FrozenTestInventoryV2;
use codex_validation_contracts::inventory_v2::InventoryDeclarationV2;
use codex_validation_contracts::inventory_v2::PredecessorArtifactReconciliationV1;
use codex_validation_contracts::inventory_v2::ProvenanceReceiptV1;
use codex_validation_contracts::inventory_v2::SchemaResourceRefV1;
use codex_validation_contracts::inventory_v2::Stage2IncorrectBehaviorIdsV1;
use codex_validation_contracts::inventory_v2::TestReplacementLedgerV2;
use codex_validation_contracts::inventory_v2::frozen_baseline_obligation_id;
use codex_validation_contracts::inventory_v2::missing_baseline_declaration_id;
use codex_validation_contracts::inventory_v2::missing_baseline_obligation_id;
use codex_validation_contracts::inventory_v2::validate_inventory_ledger_predecessor_closure;
use codex_validation_contracts::inventory_v2::validate_inventory_ledger_predecessor_closure_with_recapture;
use codex_validation_contracts::inventory_v2::validate_inventory_ledger_predecessor_closure_with_recaptures;
use codex_validation_contracts::path::StrictRepositoryPathV1;
use codex_validation_contracts::receipts::ConfirmedFailureClassificationV1;
use codex_validation_contracts::receipts::ConfirmedFailureReceiptV1;
use codex_validation_contracts::receipts::ConfirmedPassClassificationV1;
use codex_validation_contracts::receipts::ConfirmedPassReceiptV1;
use codex_validation_contracts::receipts::ExecutedOutcomeKindV1;
use codex_validation_contracts::receipts::ExecutedOutcomeV1;
use codex_validation_contracts::receipts::IntendedExecutionProjectionV1;
use codex_validation_contracts::receipts::RelevantMutationClassificationV1;
use codex_validation_contracts::receipts::RelevantMutationReceiptV1;
use codex_validation_contracts::receipts::TrustedDefectReceiptV1;
use codex_validation_contracts::receipts::ValidationAttemptClassificationV1;
use codex_validation_contracts::receipts::ValidationReceiptProjectionV1;
use codex_validation_contracts::recovery::CanonicalParameterProjectionV1;
use codex_validation_contracts::recovery::InventoryRecoveryAuthorityV1;
use codex_validation_contracts::recovery::RecoveredChildSourceV1;
use codex_validation_contracts::recovery::RecoveryTransitionReceiptV1;
use codex_validation_contracts::runner::RunnerSelectorV1;
use codex_validation_contracts::runner::TestRouteIdV1;
use codex_validation_contracts::selection::ActionIdV1;
use codex_validation_contracts::selection::ExecutableIdentityV1;
use codex_validation_contracts::selection::InventoryAuthorityRefV1;
use codex_validation_contracts::selection::SelectionRequestV1;
use codex_validation_contracts::selection::SelectionV1;
use codex_validation_contracts::selection::TestIdV1;
use codex_validation_contracts::selection::ValidationIdV1;
use hmac::Hmac;
use hmac::Mac;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::process::Command;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Vectors {
    canonical_parameter_negative_vectors: Vec<EncodedJsonVector>,
    executable_identity_vectors: Vec<Value>,
    inventory_declaration_vectors: Vec<InventoryDeclarationVector>,
    proof_hash_vectors: Vec<HashVector>,
    provenance_receipt_vectors: Vec<ProvenanceReceiptV1>,
    repository_path_vectors: PathVectors,
    replacement_ledger_vectors: Vec<TestReplacementLedgerV2>,
    runner_selector_vectors: Vec<Value>,
    rust_cfg_contract_vectors: Vec<RustCfgContractVector>,
    schema_files: Vec<String>,
    schema_version: u8,
    selection_contract_vectors: Vec<SelectionV1>,
    selection_request_vectors: SelectionRequestVectors,
    validation_receipt_vectors: Vec<ValidationReceiptProjectionV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryDeclarationVector {
    entry_tamper: InventoryDeclarationV2,
    frozen_declaration: InventoryDeclarationV2,
    frozen_obligation_id: String,
    missing_declaration: InventoryDeclarationV2,
    provenance_tamper: InventoryDeclarationV2,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RustCfgContractVector {
    applicability_result: Value,
    cargo_build_context_observation: Value,
    cargo_target_context_spec: Value,
    platform_applicability: Value,
    platform_applicability_sha256: String,
    rust_cfg_expression: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodedJsonVector {
    case: String,
    value_json_base64url: String,
}

#[test]
fn validation_receipt_classifies_partial_outcomes_without_reusing_discarded_passes() {
    let hash = |digit: char| {
        Sha256HexV1::parse(std::iter::repeat_n(digit, 64).collect::<String>())
            .expect("fixture hash")
    };
    let validation_id = ValidationIdV1::parse("rust.nextest.workspace").expect("validation ID");
    let action = ExecutableIdentityV1::Action {
        action_id: ActionIdV1::parse("action.one").expect("action ID"),
        validation_id: validation_id.clone(),
    };
    let test = ExecutableIdentityV1::Test {
        route_id: TestRouteIdV1::RustNextest,
        test_id: TestIdV1::parse("suite::one").expect("test ID"),
        validation_id: validation_id.clone(),
    };
    let intended = IntendedExecutionProjectionV1 {
        attempt_id: "attempt.partial-failure".to_owned(),
        executable_identities: vec![action.clone(), test.clone()],
        intended_count: 2,
        inventory_authority: InventoryAuthorityRefV1 {
            path: StrictRepositoryPathV1::parse(
                ".codex/validation/frozen-test-inventory-v2.json".to_owned(),
            )
            .expect("authority path"),
            raw_sha256: hash('1'),
            semantic_sha256: hash('2'),
            self_hash: hash('3'),
        },
        selection_v1_sha256: hash('4'),
        validation_id: validation_id.clone(),
    };
    let intended_hash = intended
        .semantic_sha256()
        .expect("intended projection hashes");
    let partial_failure = ValidationReceiptProjectionV1 {
        attempt_id: intended.attempt_id.clone(),
        classification: ValidationAttemptClassificationV1::ConfirmedValidationFailure,
        executed_count: 1,
        intended_execution_projection: intended.clone(),
        intended_execution_projection_sha256: intended_hash.clone(),
        mismatch_codes: vec!["runner-interrupted-after-confirmed-failure".to_owned()],
        outcomes: vec![ExecutedOutcomeV1 {
            execution_id: "execution.failed".to_owned(),
            identity: action.clone(),
            outcome: ExecutedOutcomeKindV1::Failed,
        }],
        schema_version: 1,
        selected_count: 2,
        started_count: 1,
        terminal_count: 1,
        validation_id: validation_id.clone(),
    };
    partial_failure
        .validate()
        .expect("a confirmed failed subset survives a later runner interruption");

    let mut false_pass = partial_failure.clone();
    false_pass.classification = ValidationAttemptClassificationV1::ConfirmedPass;
    assert!(
        false_pass.validate().is_err(),
        "a partial result cannot pass"
    );

    let mut failed_pre_result = partial_failure.clone();
    failed_pre_result.classification = ValidationAttemptClassificationV1::PreResultError;
    assert!(
        failed_pre_result.validate().is_err(),
        "a confirmed failed outcome cannot be relabeled as a pre-result error"
    );

    let zero_pre_result = ValidationReceiptProjectionV1 {
        attempt_id: intended.attempt_id.clone(),
        classification: ValidationAttemptClassificationV1::PreResultError,
        executed_count: 0,
        intended_execution_projection: intended.clone(),
        intended_execution_projection_sha256: intended_hash.clone(),
        mismatch_codes: vec!["runner-launch-error".to_owned()],
        outcomes: vec![],
        schema_version: 1,
        selected_count: 0,
        started_count: 0,
        terminal_count: 0,
        validation_id: validation_id.clone(),
    };
    zero_pre_result
        .validate()
        .expect("a selection or launch error may precede all observed results");

    let partial_pass_pre_result = ValidationReceiptProjectionV1 {
        attempt_id: intended.attempt_id.clone(),
        classification: ValidationAttemptClassificationV1::PreResultError,
        executed_count: 1,
        intended_execution_projection: intended.clone(),
        intended_execution_projection_sha256: intended_hash.clone(),
        mismatch_codes: vec!["runner-interrupted-after-pass".to_owned()],
        outcomes: vec![ExecutedOutcomeV1 {
            execution_id: "execution.passed".to_owned(),
            identity: action,
            outcome: ExecutedOutcomeKindV1::Passed,
        }],
        schema_version: 1,
        selected_count: 2,
        started_count: 2,
        terminal_count: 1,
        validation_id: validation_id.clone(),
    };
    partial_pass_pre_result
        .validate()
        .expect("partial confirmed passes are discarded after a runner error");

    let mut complete_pre_result = partial_pass_pre_result;
    complete_pre_result.executed_count = 2;
    complete_pre_result.outcomes.push(ExecutedOutcomeV1 {
        execution_id: "execution.second-pass".to_owned(),
        identity: test,
        outcome: ExecutedOutcomeKindV1::Passed,
    });
    complete_pre_result.terminal_count = 2;
    assert!(
        complete_pre_result.validate().is_err(),
        "a complete all-pass run cannot be relabeled as a pre-result error"
    );

    let mut replayed = partial_failure;
    replayed.attempt_id = "attempt.replayed".to_owned();
    assert!(
        replayed.validate().is_err(),
        "a receipt cannot substitute a projection from another attempt"
    );
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HashVector {
    canonical_json: String,
    domain: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathVectors {
    accepted: Vec<String>,
    rejected: Vec<String>,
    rejected_utf8_base64url: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionRequestVectors {
    invalid_tokens: Vec<String>,
    valid: Vec<SelectionRequestVector>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionRequestVector {
    request: Value,
    request_sha256: String,
    token: String,
}

#[test]
fn inventory_selection_and_receipt_vectors_match_python() {
    let raw_vectors =
        include_bytes!("../../../scripts/fixtures/completion_proof_v2_cross_language_vectors.json");
    let fixture_value: Value = serde_json::from_slice(raw_vectors).expect("fixture parses");
    assert_eq!(
        canonical_jcs(&fixture_value).expect("fixture canonicalizes"),
        raw_vectors,
        "the shared vector resource must itself be canonical JCS"
    );
    let vectors: Vectors = serde_json::from_value(fixture_value).expect("closed fixture parses");
    assert_eq!(vectors.schema_version, 1);

    for vector in &vectors.canonical_parameter_negative_vectors {
        let raw_value = URL_SAFE_NO_PAD
            .decode(&vector.value_json_base64url)
            .unwrap_or_else(|error| panic!("{} must decode: {error}", vector.case));
        let value: Value = serde_json::from_slice(&raw_value)
            .unwrap_or_else(|error| panic!("{} must parse: {error}", vector.case));
        if let Ok(parameter) =
            serde_json::from_value::<CanonicalParameterProjectionV1>(value.clone())
        {
            assert!(
                parameter.validate().is_err(),
                "negative canonical-parameter vector {} unexpectedly validated: {value}",
                vector.case
            );
        }
    }

    for vector in &vectors.proof_hash_vectors {
        let value: Value =
            serde_json::from_str(&vector.canonical_json).expect("vector JSON parses");
        assert_eq!(
            canonical_jcs(&value).expect("vector canonicalizes"),
            vector.canonical_json.as_bytes(),
            "vector must include exact canonical bytes"
        );
        let actual = proof_hash(&vector.domain, &value).expect("vector hashes");
        assert_eq!(actual.as_str(), vector.sha256);
    }

    assert_eq!(vectors.inventory_declaration_vectors.len(), 1);
    for vector in &vectors.inventory_declaration_vectors {
        vector
            .frozen_declaration
            .validate()
            .expect("literal frozen declaration validates");
        assert_eq!(
            frozen_baseline_obligation_id(&vector.frozen_declaration)
                .expect("literal frozen obligation ID hashes"),
            vector.frozen_obligation_id
        );
        vector
            .missing_declaration
            .validate()
            .expect("literal mutable declaration validates");
        assert!(vector.entry_tamper.validate().is_err());
        assert!(vector.provenance_tamper.validate().is_err());
    }

    assert_eq!(vectors.executable_identity_vectors.len(), 8);
    for value in vectors.executable_identity_vectors {
        let identity: ExecutableIdentityV1 =
            serde_json::from_value(value).expect("closed executable identity parses");
        identity.validate().expect("executable identity validates");
    }
    assert_eq!(vectors.runner_selector_vectors.len(), 8);
    for value in vectors.runner_selector_vectors {
        let selector: RunnerSelectorV1 =
            serde_json::from_value(value).expect("closed runner selector parses");
        selector.validate().expect("runner selector validates");
    }

    assert_eq!(vectors.rust_cfg_contract_vectors.len(), 1);
    for vector in vectors.rust_cfg_contract_vectors {
        let expression: RustCfgExpressionV1 =
            serde_json::from_value(vector.rust_cfg_expression).expect("Rust cfg expression parses");
        expression
            .validate()
            .expect("Rust cfg expression validates");
        let context: CargoTargetContextSpecV1 =
            serde_json::from_value(vector.cargo_target_context_spec)
                .expect("Cargo target context parses");
        context.validate().expect("Cargo target context validates");
        let observation: CargoBuildContextObservationV1 =
            serde_json::from_value(vector.cargo_build_context_observation)
                .expect("Cargo build-context observation parses");
        observation
            .validate()
            .expect("Cargo build-context observation validates");
        let platform: PlatformApplicabilityV1 =
            serde_json::from_value(vector.platform_applicability)
                .expect("platform applicability parses");
        assert_eq!(
            platform
                .semantic_sha256()
                .expect("platform applicability hashes")
                .as_str(),
            vector.platform_applicability_sha256
        );
        let result: ApplicabilityResultV1 = serde_json::from_value(vector.applicability_result)
            .expect("applicability result parses");
        result.validate().expect("applicability result validates");
        result
            .validate_for(true, &platform)
            .expect("Rust applicability shape validates");
        assert_eq!(
            result.cargo_target_context_spec_sha256,
            Some(context.context_sha256)
        );
        assert_eq!(
            result.cargo_build_context_observation_sha256,
            Some(observation.observation_sha256)
        );
    }

    for vector in vectors.selection_request_vectors.valid {
        let request: SelectionRequestV1 =
            serde_json::from_value(vector.request).expect("SelectionRequestV1 parses");
        request.validate().expect("SelectionRequestV1 validates");
        assert_eq!(request.encode_token().expect("token encodes"), vector.token);
        assert_eq!(
            request.request_sha256().expect("request hashes").as_str(),
            vector.request_sha256
        );
        assert_eq!(
            SelectionRequestV1::decode_token(&vector.token).expect("token decodes"),
            request
        );
    }
    for token in vectors.selection_request_vectors.invalid_tokens {
        assert!(
            SelectionRequestV1::decode_token(&token).is_err(),
            "{token:?}"
        );
    }

    assert_eq!(vectors.provenance_receipt_vectors.len(), 1);
    for receipt in vectors.provenance_receipt_vectors {
        receipt
            .validate()
            .expect("typed provenance receipt validates");
    }
    assert_eq!(vectors.selection_contract_vectors.len(), 1);
    for selection in vectors.selection_contract_vectors {
        selection.validate().expect("SelectionV1 validates");
        let mut substituted = serde_json::to_value(&selection).expect("selection serializes");
        substituted["resolved_entries"][0]["inventory_entry_semantic_sha256"] =
            substituted["resolved_entries"][0]["inventory_entry"]["platform_applicability_sha256"]
                .clone();
        let entries = substituted["resolved_entries"].clone();
        substituted["resolved_entries_sha256"] = Value::String(
            proof_hash(SelectionV1::ENTRY_SET_HASH_DOMAIN, &entries)
                .expect("substituted entry set hashes")
                .to_string(),
        );
        let substituted: SelectionV1 =
            serde_json::from_value(substituted).expect("substituted selection parses");
        assert!(
            substituted.validate().is_err(),
            "an applicability hash cannot substitute for the full executable inventory entry hash"
        );
    }
    assert_eq!(vectors.validation_receipt_vectors.len(), 1);
    for receipt in vectors.validation_receipt_vectors {
        receipt
            .validate()
            .expect("partial-pass pre-result receipt validates and discards its passes");
    }
    assert_eq!(vectors.replacement_ledger_vectors.len(), 3);
    for ledger in &vectors.replacement_ledger_vectors {
        ledger.validate().expect("Stage2 ledger vector validates");
    }
    let mut pending_with_review =
        serde_json::to_value(&vectors.replacement_ledger_vectors[0]).expect("ledger serializes");
    pending_with_review["rows"][0]["disposition"]["stage2_incorrect_behavior_ids"] = json!([]);
    refresh_ledger_hashes(&mut pending_with_review);
    assert!(
        serde_json::from_value::<TestReplacementLedgerV2>(pending_with_review)
            .expect("ledger parses")
            .validate()
            .is_err(),
        "a pending replacement cannot claim a reviewed no-defect state"
    );
    let mut accepted_without_review =
        serde_json::to_value(&vectors.replacement_ledger_vectors[1]).expect("ledger serializes");
    accepted_without_review["rows"][0]["disposition"]["stage2_incorrect_behavior_ids"] =
        Value::Null;
    refresh_ledger_hashes(&mut accepted_without_review);
    assert!(
        serde_json::from_value::<TestReplacementLedgerV2>(accepted_without_review)
            .expect("ledger parses")
            .validate()
            .is_err(),
        "an accepted replacement must record [] or its nonempty reviewed defect IDs"
    );

    let hash = |digit: char| {
        Sha256HexV1::parse(std::iter::repeat_n(digit, 64).collect::<String>())
            .expect("fixture hash")
    };
    let mut defect_receipt = TrustedDefectReceiptV1 {
        baseline_ids: vec!["baseline.one".to_owned()],
        baseline_obligation_ids: vec!["baseline.one".to_owned()],
        defect_id: "stage2::inventory::copied-proof".to_owned(),
        incorrect_behavior: "Copied text was accepted without runtime evidence.".to_owned(),
        failure: ConfirmedFailureReceiptV1 {
            attempt_id: "attempt.failure".to_owned(),
            classification: ConfirmedFailureClassificationV1::ConfirmedValidationFailure,
            execution_ids: vec!["execution.failure".to_owned()],
            focused_projection_sha256: hash('1'),
            mutation_epoch: 7,
            workspace_fingerprint: hash('2'),
        },
        mutation: RelevantMutationReceiptV1 {
            changed_input_leaf_ids: vec!["input.product".to_owned()],
            classification: RelevantMutationClassificationV1::RelevantNonTestProductRuntime,
            from_epoch: 7,
            input_contract_set_sha256: hash('3'),
            production_delta_sha256: hash('4'),
            to_epoch: 8,
        },
        pass: ConfirmedPassReceiptV1 {
            attempt_id: "attempt.pass".to_owned(),
            classification: ConfirmedPassClassificationV1::ConfirmedPass,
            execution_ids: vec!["execution.pass".to_owned()],
            focused_projection_sha256: hash('5'),
            mutation_epoch: 8,
            workspace_fingerprint: hash('6'),
        },
        receipt_sha256: hash('0'),
        replacement_edge_ids: vec!["edge.one".to_owned()],
        resolved_entry_set_sha256: hash('7'),
        schema_version: 1,
        selection_v1_sha256: hash('8'),
    };
    let mut receipt_projection =
        serde_json::to_value(&defect_receipt).expect("receipt projection serializes");
    receipt_projection
        .as_object_mut()
        .expect("receipt is an object")
        .remove("receipt_sha256");
    defect_receipt.receipt_sha256 =
        proof_hash(TrustedDefectReceiptV1::HASH_DOMAIN, &receipt_projection)
            .expect("receipt hashes");
    defect_receipt
        .validate()
        .expect("failure, relevant mutation, and fresh pass receipt validates");
    let mut copied_text = defect_receipt.clone();
    copied_text.incorrect_behavior.push_str(" altered");
    assert!(
        copied_text.validate().is_err(),
        "the incorrect-behavior description is authenticated"
    );
    let mut unchanged_retry = defect_receipt.clone();
    unchanged_retry.pass.mutation_epoch = 7;
    assert!(
        unchanged_retry.validate().is_err(),
        "an unchanged same-epoch retry is not trusted evidence"
    );

    for path in vectors.repository_path_vectors.accepted {
        assert_eq!(
            StrictRepositoryPathV1::parse(path.clone())
                .expect("accepted path")
                .as_str(),
            path
        );
    }
    for path in vectors.repository_path_vectors.rejected {
        assert!(
            StrictRepositoryPathV1::parse(path.clone()).is_err(),
            "unexpectedly accepted {path:?}"
        );
    }
    for encoded in vectors.repository_path_vectors.rejected_utf8_base64url {
        let path = String::from_utf8(
            URL_SAFE_NO_PAD
                .decode(encoded)
                .expect("path vector decodes"),
        )
        .expect("path vector is UTF-8");
        assert!(StrictRepositoryPathV1::parse(path).is_err());
    }

    for relative in vectors.schema_files {
        let raw = schema_bytes(&relative);
        let schema: Value = serde_json::from_slice(raw).expect("schema JSON parses");
        assert_eq!(
            canonical_jcs(&schema).expect("schema canonicalizes"),
            raw,
            "{relative} must be stored as exact canonical JCS"
        );
        assert_eq!(
            schema.get("$schema").and_then(Value::as_str),
            Some("https://json-schema.org/draft/2020-12/schema"),
            "{relative}"
        );
        assert_closed_object_schemas(&schema, &relative);
    }
}

#[test]
fn stage2_wire_state_preserves_null_empty_nonempty_and_rejects_absent() {
    assert!(matches!(
        serde_json::from_value::<Stage2IncorrectBehaviorIdsV1>(Value::Null)
            .expect("null Stage2 state parses"),
        Stage2IncorrectBehaviorIdsV1::Unreviewed(_)
    ));
    assert!(matches!(
        serde_json::from_value::<Stage2IncorrectBehaviorIdsV1>(json!([]))
            .expect("empty reviewed Stage2 state parses"),
        Stage2IncorrectBehaviorIdsV1::Reviewed(ids) if ids.is_empty()
    ));
    assert!(matches!(
        serde_json::from_value::<Stage2IncorrectBehaviorIdsV1>(json!(["defect.one"]))
            .expect("nonempty reviewed Stage2 state parses"),
        Stage2IncorrectBehaviorIdsV1::Reviewed(ids) if ids == ["defect.one"]
    ));

    let fixture = shared_fixture_value();
    let mut absent = fixture["replacement_ledger_vectors"][0].clone();
    absent["rows"][0]["disposition"]
        .as_object_mut()
        .expect("replacement disposition is an object")
        .remove("stage2_incorrect_behavior_ids");
    assert!(
        serde_json::from_value::<TestReplacementLedgerV2>(absent).is_err(),
        "an absent Stage2 field must not collapse into the explicit null state"
    );
}

#[test]
fn declaration_ids_bind_the_exact_entry_and_typed_provenance() {
    let fixture = shared_fixture_value();
    let entry = serde_json::from_value(
        fixture["selection_contract_vectors"][0]["resolved_entries"][0]["inventory_entry"].clone(),
    )
    .expect("inventory entry parses");
    let provenance: ProvenanceReceiptV1 =
        serde_json::from_value(fixture["provenance_receipt_vectors"][0].clone())
            .expect("provenance parses");
    let declaration = InventoryDeclarationV2::MissingBaseline {
        declaration_id: missing_baseline_declaration_id(&entry, &provenance)
            .expect("declaration ID hashes"),
        obligation_id: missing_baseline_obligation_id(&entry, &provenance)
            .expect("obligation ID hashes"),
        entry,
        source_provenance: provenance,
    };
    declaration.validate().expect("exact declaration validates");

    let mut stale_entry = serde_json::to_value(&declaration).expect("declaration serializes");
    stale_entry["entry"]["execution_input_contract_sha256"] = json!("d".repeat(64));
    let stale_entry: InventoryDeclarationV2 =
        serde_json::from_value(stale_entry).expect("mutated declaration parses");
    assert!(stale_entry.validate().is_err());

    let mut stale_provenance = serde_json::to_value(&declaration).expect("declaration serializes");
    stale_provenance["source_provenance"]["evidence_sha256"] = json!("e".repeat(64));
    let mut receipt_projection = stale_provenance["source_provenance"].clone();
    receipt_projection
        .as_object_mut()
        .expect("provenance is an object")
        .remove("receipt_sha256");
    stale_provenance["source_provenance"]["receipt_sha256"] = json!(
        proof_hash(ProvenanceReceiptV1::HASH_DOMAIN, &receipt_projection)
            .expect("provenance rehashes")
            .to_string()
    );
    let stale_provenance: InventoryDeclarationV2 =
        serde_json::from_value(stale_provenance).expect("mutated declaration parses");
    assert!(stale_provenance.validate().is_err());
}

#[test]
fn active_host_applicability_is_authority_authenticated() {
    let (authority_value, authentication_key) = active_host_authority_value();
    let authority: ActiveHostApplicabilityAuthorityV1 =
        serde_json::from_value(authority_value.clone()).expect("authority parses");
    authority
        .validate_authenticated(&authentication_key)
        .expect("authority authentication validates");
    assert!(authority.validate_authenticated(&[b'W'; 32]).is_err());

    let mut substituted = authority_value.clone();
    substituted["body"]["target_applicability_projection"]["host"] = json!("linux");
    let substituted: ActiveHostApplicabilityAuthorityV1 =
        serde_json::from_value(substituted).expect("substituted authority parses");
    assert!(
        substituted
            .validate_authenticated(&authentication_key)
            .is_err()
    );

    let fixture = shared_fixture_value();
    let mut provenance = fixture["provenance_receipt_vectors"][0].clone();
    provenance["kind"] = json!("generated");
    let mut provenance_projection = provenance.clone();
    provenance_projection
        .as_object_mut()
        .expect("provenance is an object")
        .remove("receipt_sha256");
    provenance["receipt_sha256"] = json!(
        proof_hash(ProvenanceReceiptV1::HASH_DOMAIN, &provenance_projection)
            .expect("provenance rehashes")
            .to_string()
    );
    let exception_projection = json!({
        "active_host_authority": authority_value,
        "provenance_receipt": provenance,
        "tag": "generated",
    });
    let exception = json!({
        "active_host_authority": exception_projection["active_host_authority"],
        "kind": "accepted",
        "provenance_receipt": exception_projection["provenance_receipt"],
        "receipt_sha256": proof_hash(
            "kd4.accepted-exception-receipt.v1",
            &exception_projection,
        )
        .expect("accepted exception hashes")
        .to_string(),
        "tag": "generated",
    });
    let mut ledger_value = json!({
        "format_id": "kd4.test-replacement-ledger.v2",
        "inventory_authority": exception_projection["active_host_authority"]["body"]["inventory_authority"],
        "rows": [{
            "baseline_id": "baseline.one",
            "disposition": {"exception": exception, "kind": "exception"},
            "obligation_id": "obligation.one",
        }],
        "schema_version": 2,
        "semantic_sha256": "0".repeat(64),
        "self_hash": "0".repeat(64),
        "trusted_defect_receipts": null,
    });
    refresh_ledger_hashes(&mut ledger_value);
    serde_json::from_value::<TestReplacementLedgerV2>(ledger_value.clone())
        .expect("accepted-exception ledger parses")
        .validate()
        .expect("complete authority-bound exception receipt validates");
    ledger_value["rows"][0]["disposition"]["exception"]["active_host_authority"]["authentication_tag"] =
        json!(STANDARD_NO_PAD.encode([b'M'; 32]));
    refresh_ledger_hashes(&mut ledger_value);
    assert!(
        serde_json::from_value::<TestReplacementLedgerV2>(ledger_value)
            .expect("tampered accepted-exception ledger parses")
            .validate()
            .is_err(),
        "the accepted-exception receipt must close over the complete authenticated authority"
    );
}

#[test]
fn frozen_unittest_ledger_partitions_hidden_rows_from_executable_recapture() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("validation-contracts is nested under codex-rs");
    let frozen_inventory: Value = serde_json::from_slice(
        &std::fs::read(repo_root.join(".codex/validation/frozen-test-inventory-v1.json"))
            .expect("frozen V1 inventory is readable"),
    )
    .expect("frozen V1 inventory parses");
    let predecessor_ledger: Value = serde_json::from_slice(
        &std::fs::read(repo_root.join(".codex/validation/test-replacements-v1.json"))
            .expect("frozen V1 replacement ledger is readable"),
    )
    .expect("frozen V1 replacement ledger parses");

    let unittest_parent_ids = frozen_inventory["tests"]
        .as_array()
        .expect("frozen V1 inventory has tests")
        .iter()
        .filter(|entry| entry["framework"] == "python-unittest")
        .map(|entry| {
            entry["baseline_id"]
                .as_str()
                .expect("unittest baseline ID is a string")
                .to_owned()
        })
        .collect::<BTreeSet<_>>();
    let hidden_parent_ids = unittest_parent_ids
        .iter()
        .filter(|baseline_id| baseline_id.starts_with("hidden-at-freeze-v1::python-unittest::"))
        .cloned()
        .collect::<BTreeSet<_>>();
    let executable_parent_ids = unittest_parent_ids
        .difference(&hidden_parent_ids)
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(unittest_parent_ids.len(), 909);
    assert_eq!(executable_parent_ids.len(), 893);
    assert_eq!(hidden_parent_ids.len(), 16);
    assert!(executable_parent_ids.is_disjoint(&hidden_parent_ids));
    assert_eq!(
        executable_parent_ids
            .union(&hidden_parent_ids)
            .cloned()
            .collect::<BTreeSet<_>>(),
        unittest_parent_ids
    );
    let hidden_parent_ids_for_hash = hidden_parent_ids.iter().cloned().collect::<Vec<_>>();
    assert_eq!(
        proof_hash(
            "kd4.unittest-hidden-ledger-parent-ids.v1",
            &hidden_parent_ids_for_hash,
        )
        .expect("hidden unittest set hashes")
        .as_str(),
        "936330f9e9a23c8d628f651a1ed31b3f4ea836a06cf152a6acf09cff898ebc40"
    );

    let predecessor_rows = predecessor_ledger["rows"]
        .as_array()
        .expect("frozen V1 ledger has rows")
        .iter()
        .filter_map(|row| {
            let baseline_id = row["baseline_id"].as_str()?;
            unittest_parent_ids
                .contains(baseline_id)
                .then_some((baseline_id.to_owned(), row))
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        predecessor_rows.keys().cloned().collect::<BTreeSet<_>>(),
        unittest_parent_ids,
        "the predecessor ledger must retain every one of the 909 unittest identities"
    );
    let replacement_ids = predecessor_rows
        .iter()
        .filter(|(_, row)| row["resolution"] == "replacement")
        .map(|(baseline_id, _)| baseline_id.clone())
        .collect::<Vec<_>>();
    let executable_replacement_ids = replacement_ids
        .iter()
        .filter(|baseline_id| executable_parent_ids.contains(*baseline_id))
        .cloned()
        .collect::<Vec<_>>();
    let unresolved_ids = predecessor_rows
        .iter()
        .filter(|(_, row)| row["resolution"] == "unresolved")
        .map(|(baseline_id, _)| baseline_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(replacement_ids.len(), 536);
    assert_eq!(executable_replacement_ids.len(), 520);
    assert_eq!(unresolved_ids.len(), 373);
    assert_eq!(
        proof_hash("kd4.unittest-parent-replacement-ids.v1", &replacement_ids,)
            .expect("all replacement unittest IDs hash")
            .as_str(),
        "59202883ef32488ae9a488345b2401400794d961e5d539c58fd6364e86f25fe1"
    );
    assert_eq!(
        proof_hash(
            "kd4.unittest-parent-replacement-ids.v1",
            &executable_replacement_ids,
        )
        .expect("executable replacement unittest IDs hash")
        .as_str(),
        "208d6735c1413d94c503a453331889fe5709b8eca540cf9c13993c42f9bb8cf7"
    );
    assert_eq!(
        proof_hash("kd4.unittest-parent-unresolved-ids.v1", &unresolved_ids)
            .expect("unresolved unittest IDs hash")
            .as_str(),
        "d1e66a89d1a943b60f6516bc9550102306919d6ae673bee4027595a3df8036f7"
    );
    for hidden_parent_id in &hidden_parent_ids {
        let row = predecessor_rows
            .get(hidden_parent_id)
            .expect("hidden parent has its frozen ledger row");
        assert_eq!(row["resolution"], "replacement");
        let native_id = hidden_parent_id
            .strip_prefix("hidden-at-freeze-v1::")
            .expect("hidden identity has the authenticated prefix");
        assert_eq!(row["replacement_ids"], json!([native_id]));
        assert!(!executable_parent_ids.contains(hidden_parent_id));
    }
    assert!(
        codex_validation_contracts::recovery::UNITTEST_RECAPTURE_SOURCE_SITE_MANIFEST_SHA256
            .is_none(),
        "no synthetic accepted packet may replace the unavailable freeze-site authority"
    );

    let python = if cfg!(windows) { "python" } else { "python3" };
    let python_constants = Command::new(python)
        .args([
            "-c",
            "from scripts.completion_proof_inventory_v2 import UNITTEST_V1_HIDDEN_PARENT_IDS_SHA256, UNITTEST_V1_REPLACEMENT_PARENT_IDS_SHA256, UNITTEST_V1_EXECUTABLE_REPLACEMENT_PARENT_IDS_SHA256, UNITTEST_V1_UNRESOLVED_PARENT_IDS_SHA256; print('|'.join([UNITTEST_V1_HIDDEN_PARENT_IDS_SHA256, UNITTEST_V1_REPLACEMENT_PARENT_IDS_SHA256, UNITTEST_V1_EXECUTABLE_REPLACEMENT_PARENT_IDS_SHA256, UNITTEST_V1_UNRESOLVED_PARENT_IDS_SHA256]))",
        ])
        .current_dir(repo_root)
        .output()
        .expect("Python exposes the unittest partition anchors");
    assert!(
        python_constants.status.success(),
        "Python partition anchor lookup failed: {}",
        String::from_utf8_lossy(&python_constants.stderr)
    );
    assert_eq!(
        String::from_utf8(python_constants.stdout)
            .expect("Python partition anchors are UTF-8")
            .trim(),
        "936330f9e9a23c8d628f651a1ed31b3f4ea836a06cf152a6acf09cff898ebc40|59202883ef32488ae9a488345b2401400794d961e5d539c58fd6364e86f25fe1|208d6735c1413d94c503a453331889fe5709b8eca540cf9c13993c42f9bb8cf7|d1e66a89d1a943b60f6516bc9550102306919d6ae673bee4027595a3df8036f7"
    );
}

#[test]
fn full_scale_python_artifact_bundle_validates_in_rust_with_hash_parity() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("validation-contracts is nested under codex-rs");
    let python = if cfg!(windows) { "python" } else { "python3" };
    let output = Command::new(python)
        .args([
            "-c",
            "from scripts.test_completion_proof_inventory_v2 import _full_scale_integration_fixture_v2; from scripts.completion_proof_inventory_v2 import canonical_jcs; import sys; sys.stdout.buffer.write(canonical_jcs(_full_scale_integration_fixture_v2()))",
        ])
        .current_dir(repo_root)
        .output()
        .expect("Python emits the temporary full-scale bundle");
    assert!(
        output.status.success(),
        "Python fixture generation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bundle: Value =
        serde_json::from_slice(&output.stdout).expect("full-scale bundle parses as JSON");
    assert_eq!(
        bundle["counts"],
        json!({
            "inventory_declarations": 15_547,
            "ledger_rows": 15_547,
            "recovery_records": 2,
        })
    );
    let expected_artifact_hashes = json!({
        "inventory": "e0ddc09c36fe346e511256a6fd1011f3892606e3150e2c045ccae14cdfb6807a",
        "ledger": "651f98d60ca3e0c6755dfecf87c46e251494ad6f05397aa502ca1a4c4a000eb6",
        "recovery": "2d6e7ee1aa26ce1094f93f38152f63fc54e6db7c569a8e7acf51a709896a1712",
    });
    assert_eq!(bundle["artifact_sha256"], expected_artifact_hashes);
    for name in ["inventory", "ledger", "recovery"] {
        let actual = format!(
            "{:x}",
            Sha256::digest(canonical_jcs(&bundle[name]).expect("artifact canonicalizes"))
        );
        assert_eq!(actual, expected_artifact_hashes[name]);
    }
    assert_eq!(
        bundle["inventory"]["authority"]["semantic_sha256"],
        "46b2bde1834a893f7d9851a92cb10e3ac2b2348555288a873f14941040d79288"
    );
    assert_eq!(
        bundle["ledger"]["semantic_sha256"],
        "134ed74a5ac1ca9c4d3c5080ec441439b1081895f31f180214f2432dee490db8"
    );
    assert_eq!(
        bundle["recovery"]["semantic_sha256"],
        "34b80e4f41d380d6e9c83e3182a80f7d35250b3284973dee75dcb67802a6dde0"
    );

    let inventory: FrozenTestInventoryV2 =
        serde_json::from_value(bundle["inventory"].clone()).expect("inventory parses");
    let ledger: TestReplacementLedgerV2 =
        serde_json::from_value(bundle["ledger"].clone()).expect("ledger parses");
    let recovery: InventoryRecoveryAuthorityV1 =
        serde_json::from_value(bundle["recovery"].clone()).expect("recovery parses");
    let transition_receipts: Vec<RecoveryTransitionReceiptV1> =
        serde_json::from_value(bundle["transition_receipts"].clone())
            .expect("transition receipts parse");
    recovery.validate().expect("recovery validates in Rust");
    inventory.validate().expect("inventory validates in Rust");
    ledger.validate().expect("ledger validates in Rust");
    let issuer = ActiveHostApplicabilityIssuerV1::new(
        "12345678-1234-1234-1234-123456789abc".to_owned(),
        [b'K'; 32],
        Vec::new(),
    )
    .expect("trusted issuer constructs");
    let authority = issuer
        .issue(
            &inventory,
            HostTokenV1::Windows,
            STANDARD_NO_PAD.encode([b'N'; 32]),
        )
        .expect("complete active-host projection issues");
    issuer
        .validate_complete_authority(&authority, &inventory)
        .expect("issuer verifies complete inventory coverage");
    let mut incomplete_authority = authority.clone();
    incomplete_authority
        .body
        .target_applicability_projection
        .entries
        .pop();
    incomplete_authority
        .body
        .target_applicability_projection_sha256 = incomplete_authority
        .body
        .target_applicability_projection
        .semantic_sha256()
        .expect("incomplete projection hash computes");
    incomplete_authority.authority_sha256 = proof_hash(
        ActiveHostApplicabilityAuthorityV1::HASH_DOMAIN,
        &incomplete_authority.body,
    )
    .expect("incomplete authority hash computes");
    let authentication_payload = json!({
        "authority_sha256": &incomplete_authority.authority_sha256,
        "body": &incomplete_authority.body,
        "key_id": &incomplete_authority.key_id,
        "schema_version": incomplete_authority.schema_version,
    });
    let mut mac = Hmac::<Sha256>::new_from_slice(&[b'K'; 32]).expect("32-byte HMAC key is valid");
    mac.update(ActiveHostApplicabilityAuthorityV1::AUTHENTICATION_DOMAIN.as_bytes());
    mac.update(&[0]);
    mac.update(
        &canonical_jcs(&authentication_payload).expect("authentication payload canonicalizes"),
    );
    incomplete_authority.authentication_tag = STANDARD_NO_PAD.encode(mac.finalize().into_bytes());
    incomplete_authority
        .validate_authenticated(&[b'K'; 32])
        .expect("arbitrary incomplete projection remains correctly authenticated");
    assert!(
        issuer
            .validate_complete_authority(&incomplete_authority, &inventory)
            .is_err(),
        "trusted issuer rejects validly signed but incomplete projection"
    );
    let recovery_raw =
        canonical_jcs(&bundle["recovery"]).expect("full-scale recovery authority canonicalizes");
    let doctest_recapture_raw = canonical_jcs(&bundle["doctest_recapture"])
        .expect("full-scale doctest recapture packet canonicalizes");
    let mut noncanonical_recovery_raw = recovery_raw.clone();
    noncanonical_recovery_raw.push(b'\n');
    assert!(
        validate_inventory_ledger_predecessor_closure_with_recapture(
            &inventory,
            &ledger,
            &noncanonical_recovery_raw,
            &transition_receipts,
            &issuer,
            Some(&doctest_recapture_raw),
        )
        .is_err(),
        "closure rejects noncanonical recovery bytes instead of reconstructing a raw hash"
    );
    assert!(
        validate_inventory_ledger_predecessor_closure(
            &inventory,
            &ledger,
            &recovery_raw,
            &transition_receipts,
            &issuer,
        )
        .is_err(),
        "resolved doctest closure rejects an omitted typed recapture packet"
    );
    validate_inventory_ledger_predecessor_closure_with_recapture(
        &inventory,
        &ledger,
        &recovery_raw,
        &transition_receipts,
        &issuer,
        Some(&doctest_recapture_raw),
    )
    .expect("full-scale predecessor closure validates in Rust");
    validate_inventory_ledger_predecessor_closure_with_recaptures(
        &inventory,
        &ledger,
        &recovery_raw,
        &transition_receipts,
        &issuer,
        Some(&doctest_recapture_raw),
        None,
        None,
    )
    .expect("strongest closure preserves pending-unittest Python/Rust parity");
    assert!(
        validate_inventory_ledger_predecessor_closure_with_recaptures(
            &inventory,
            &ledger,
            &recovery_raw,
            &transition_receipts,
            &issuer,
            Some(&doctest_recapture_raw),
            Some(&doctest_recapture_raw),
            None,
        )
        .is_err(),
        "strongest closure rejects a foreign typed packet in the unittest slot"
    );

    let mut authority_tampered_receipts_value = bundle["transition_receipts"].clone();
    let authority_tampered_receipt = authority_tampered_receipts_value
        .as_array_mut()
        .expect("transition receipts are an array")
        .iter_mut()
        .find(|receipt| {
            receipt["recapture_receipt_sha256"] == bundle["doctest_recapture"]["receipt_sha256"]
        })
        .expect("fixture has a doctest transition receipt");
    authority_tampered_receipt["authority_before_semantic_sha256"] = json!("f".repeat(64));
    refresh_transition_receipt_hash(authority_tampered_receipt);
    let authority_tampered_receipt_hash = authority_tampered_receipt["receipt_sha256"].clone();
    let authority_tampered_receipts: Vec<RecoveryTransitionReceiptV1> =
        serde_json::from_value(authority_tampered_receipts_value)
            .expect("authority-tampered receipts parse");
    for receipt in &authority_tampered_receipts {
        receipt
            .validate()
            .expect("authority-tampered receipt remains internally valid");
    }

    let mut authority_tampered_recovery_value = bundle["recovery"].clone();
    let authority_tampered_doctest_record = authority_tampered_recovery_value["records"]
        .as_array_mut()
        .expect("recovery records are an array")
        .iter_mut()
        .find(|record| record["kind"] == "doctest")
        .expect("fixture has a doctest recovery");
    authority_tampered_doctest_record["transition_receipt_sha256"] =
        authority_tampered_receipt_hash.clone();
    refresh_recovery_hashes(&mut authority_tampered_recovery_value);
    let authority_tampered_recovery_raw = canonical_jcs(&authority_tampered_recovery_value)
        .expect("authority-tampered recovery canonicalizes");

    let mut authority_tampered_inventory_value = bundle["inventory"].clone();
    authority_tampered_inventory_value["recovery_authority"]["raw_sha256"] = json!(format!(
        "{:x}",
        Sha256::digest(&authority_tampered_recovery_raw)
    ));
    authority_tampered_inventory_value["recovery_authority"]["semantic_sha256"] =
        authority_tampered_recovery_value["semantic_sha256"].clone();
    authority_tampered_inventory_value["recovery_authority"]["self_hash"] =
        authority_tampered_recovery_value["self_hash"].clone();
    refresh_inventory_hashes(&mut authority_tampered_inventory_value);
    let authority_tampered_inventory: FrozenTestInventoryV2 =
        serde_json::from_value(authority_tampered_inventory_value.clone())
            .expect("authority-tampered inventory parses");
    authority_tampered_inventory
        .validate()
        .expect("authority-tampered inventory remains internally valid");

    let mut authority_tampered_ledger_value = bundle["ledger"].clone();
    authority_tampered_ledger_value["inventory_authority"] = json!({
        "path": ".codex/validation/frozen-test-inventory-v2.json",
        "raw_sha256": authority_tampered_inventory_value["authority"]["raw_sha256"],
        "semantic_sha256": authority_tampered_inventory_value["authority"]["semantic_sha256"],
        "self_hash": authority_tampered_inventory_value["authority"]["self_hash"],
    });
    let authority_tampered_container = authority_tampered_ledger_value["rows"]
        .as_array_mut()
        .expect("ledger rows are an array")
        .iter_mut()
        .find(|row| row["disposition"]["kind"] == "recovered-container")
        .expect("fixture has a recovered doctest container");
    authority_tampered_container["disposition"]["transition_receipt_sha256"] =
        authority_tampered_receipt_hash;
    refresh_ledger_hashes(&mut authority_tampered_ledger_value);
    let authority_tampered_ledger: TestReplacementLedgerV2 =
        serde_json::from_value(authority_tampered_ledger_value)
            .expect("authority-tampered ledger parses");
    authority_tampered_ledger
        .validate()
        .expect("authority-tampered ledger remains internally valid");
    let error = validate_inventory_ledger_predecessor_closure_with_recaptures(
        &authority_tampered_inventory,
        &authority_tampered_ledger,
        &authority_tampered_recovery_raw,
        &authority_tampered_receipts,
        &issuer,
        Some(&doctest_recapture_raw),
        None,
        None,
    )
    .expect_err("closure rejects a self-consistently rehashed false predecessor authority");
    assert!(
        error.to_string().contains("actual predecessor authority"),
        "transition tampering must be rejected against reconstructed authority: {error}"
    );

    let mut child_tampered_recovery_value = bundle["recovery"].clone();
    let doctest_record = child_tampered_recovery_value["records"]
        .as_array_mut()
        .expect("recovery records are an array")
        .iter_mut()
        .find(|record| record["kind"] == "doctest")
        .expect("fixture has a resolved doctest record");
    doctest_record["resolution"]["child_sources"]
        .as_array_mut()
        .expect("doctest child sources are an array")[0]["executable_identity"]["validation_id"] =
        json!("rust.doctest.tampered");
    sort_recovered_children(doctest_record);
    refresh_recovery_hashes(&mut child_tampered_recovery_value);
    let child_tampered_recovery_raw = canonical_jcs(&child_tampered_recovery_value)
        .expect("child-tampered recovery authority canonicalizes");
    let child_tampered_recovery: InventoryRecoveryAuthorityV1 =
        serde_json::from_value(child_tampered_recovery_value.clone())
            .expect("child-tampered recovery parses");
    child_tampered_recovery
        .validate()
        .expect("child-tampered recovery remains internally valid");

    let mut child_tampered_inventory_value = bundle["inventory"].clone();
    child_tampered_inventory_value["recovery_authority"]["raw_sha256"] = json!(format!(
        "{:x}",
        Sha256::digest(&child_tampered_recovery_raw)
    ));
    child_tampered_inventory_value["recovery_authority"]["semantic_sha256"] =
        child_tampered_recovery_value["semantic_sha256"].clone();
    child_tampered_inventory_value["recovery_authority"]["self_hash"] =
        child_tampered_recovery_value["self_hash"].clone();
    refresh_inventory_hashes(&mut child_tampered_inventory_value);
    let child_tampered_inventory: FrozenTestInventoryV2 =
        serde_json::from_value(child_tampered_inventory_value.clone())
            .expect("child-tampered inventory parses");
    child_tampered_inventory
        .validate()
        .expect("child-tampered inventory remains internally valid");

    let mut child_tampered_ledger_value = bundle["ledger"].clone();
    child_tampered_ledger_value["inventory_authority"] = json!({
        "path": ".codex/validation/frozen-test-inventory-v2.json",
        "raw_sha256": child_tampered_inventory_value["authority"]["raw_sha256"],
        "semantic_sha256": child_tampered_inventory_value["authority"]["semantic_sha256"],
        "self_hash": child_tampered_inventory_value["authority"]["self_hash"],
    });
    refresh_ledger_hashes(&mut child_tampered_ledger_value);
    let child_tampered_ledger: TestReplacementLedgerV2 =
        serde_json::from_value(child_tampered_ledger_value).expect("child-tampered ledger parses");
    child_tampered_ledger
        .validate()
        .expect("child-tampered ledger remains internally valid");
    let error = validate_inventory_ledger_predecessor_closure_with_recapture(
        &child_tampered_inventory,
        &child_tampered_ledger,
        &child_tampered_recovery_raw,
        &transition_receipts,
        &issuer,
        Some(&doctest_recapture_raw),
    )
    .expect_err("packet closure rejects a different doctest validation identity");
    assert!(
        error
            .to_string()
            .contains("doctest recovery does not exactly materialize"),
        "tampering must be rejected by exact typed-packet child comparison: {error}"
    );

    let mut forged_value = bundle["ledger"].clone();
    forged_value["rows"][0]["obligation_id"] = json!("inventory-obligation-v2.forged");
    refresh_ledger_hashes(&mut forged_value);
    let forged: TestReplacementLedgerV2 =
        serde_json::from_value(forged_value).expect("forged ledger parses");
    forged
        .validate()
        .expect("forged ledger has internally consistent hashes");
    assert!(
        validate_inventory_ledger_predecessor_closure_with_recapture(
            &inventory,
            &forged,
            &recovery_raw,
            &transition_receipts,
            &issuer,
            Some(&doctest_recapture_raw),
        )
        .is_err(),
        "closure rejects a forged obligation even after ledger hashes are recomputed"
    );

    let source_rows = bundle["ledger"]["rows"]
        .as_array()
        .expect("ledger rows are an array")
        .iter()
        .filter(|row| row["baseline_id"].is_null())
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(source_rows.len(), 3);

    {
        let mut omitted_value = bundle["ledger"].clone();
        omitted_value["rows"]
            .as_array_mut()
            .expect("ledger rows are an array")
            .retain(|row| row["obligation_id"] != source_rows[0]["obligation_id"]);
        refresh_ledger_hashes(&mut omitted_value);
        let omitted: TestReplacementLedgerV2 =
            serde_json::from_value(omitted_value).expect("omitted ledger parses");
        omitted
            .validate()
            .expect("omitted ledger has internally consistent hashes");
        assert!(
            validate_inventory_ledger_predecessor_closure_with_recapture(
                &inventory,
                &omitted,
                &recovery_raw,
                &transition_receipts,
                &issuer,
                Some(&doctest_recapture_raw),
            )
            .is_err(),
            "closure rejects an omitted missing-baseline declaration row"
        );
    }

    {
        let mut duplicate_value = bundle["ledger"].clone();
        let mut duplicate_row = source_rows[0].clone();
        duplicate_row["disposition"] = json!({"kind": "unresolved"});
        duplicate_value["rows"]
            .as_array_mut()
            .expect("ledger rows are an array")
            .push(duplicate_row);
        sort_ledger_rows(&mut duplicate_value);
        refresh_ledger_hashes(&mut duplicate_value);
        let duplicate: TestReplacementLedgerV2 =
            serde_json::from_value(duplicate_value).expect("duplicate ledger parses");
        duplicate
            .validate()
            .expect("duplicate ledger has internally consistent hashes");
        assert!(
            validate_inventory_ledger_predecessor_closure_with_recapture(
                &inventory,
                &duplicate,
                &recovery_raw,
                &transition_receipts,
                &issuer,
                Some(&doctest_recapture_raw),
            )
            .is_err(),
            "closure rejects duplicate nonbaseline obligation rows"
        );
    }

    {
        let mut unknown_value = bundle["ledger"].clone();
        let unknown_row = unknown_value["rows"]
            .as_array_mut()
            .expect("ledger rows are an array")
            .iter_mut()
            .find(|row| row["baseline_id"].is_null())
            .expect("fixture has a missing-baseline row");
        unknown_row["obligation_id"] = json!(format!(
            "inventory-obligation-v2.missing-declaration.{}",
            "f".repeat(64)
        ));
        sort_ledger_rows(&mut unknown_value);
        refresh_ledger_hashes(&mut unknown_value);
        let unknown: TestReplacementLedgerV2 =
            serde_json::from_value(unknown_value).expect("unknown-obligation ledger parses");
        unknown
            .validate()
            .expect("unknown-obligation ledger has internally consistent hashes");
        assert!(
            validate_inventory_ledger_predecessor_closure_with_recapture(
                &inventory,
                &unknown,
                &recovery_raw,
                &transition_receipts,
                &issuer,
                Some(&doctest_recapture_raw),
            )
            .is_err(),
            "closure rejects an unknown substituted nonbaseline obligation"
        );
    }

    {
        let mut substituted_value = bundle["ledger"].clone();
        let rows = substituted_value["rows"]
            .as_array_mut()
            .expect("ledger rows are an array");
        let source_indices = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row["baseline_id"].is_null().then_some(index))
            .collect::<Vec<_>>();
        let substituted_provenance =
            rows[source_indices[1]]["disposition"]["exception"]["provenance_receipt"].clone();
        let exception = &mut rows[source_indices[0]]["disposition"]["exception"];
        exception["provenance_receipt"] = substituted_provenance;
        let exception_projection = json!({
            "active_host_authority": exception["active_host_authority"],
            "provenance_receipt": exception["provenance_receipt"],
            "tag": exception["tag"],
        });
        exception["receipt_sha256"] = json!(
            proof_hash("kd4.accepted-exception-receipt.v1", &exception_projection,)
                .expect("substituted exception receipt hashes")
                .to_string()
        );
        sort_ledger_rows(&mut substituted_value);
        refresh_ledger_hashes(&mut substituted_value);
        let substituted: TestReplacementLedgerV2 = serde_json::from_value(substituted_value)
            .expect("provenance-substituted ledger parses");
        substituted
            .validate()
            .expect("provenance-substituted ledger has internally consistent hashes");
        assert!(
            validate_inventory_ledger_predecessor_closure_with_recapture(
                &inventory,
                &substituted,
                &recovery_raw,
                &transition_receipts,
                &issuer,
                Some(&doctest_recapture_raw),
            )
            .is_err(),
            "closure rejects baseline-null exception provenance from another declaration"
        );
    }
}

#[test]
fn json_schemas_validate_serialized_rust_contract_instances() {
    let schema_paths = [
        ".codex/validation/frozen-test-inventory-v2-recoveries.schema.json",
        ".codex/validation/frozen-test-inventory-v2.schema.json",
        ".codex/validation/inventory-shared-types-v1.schema.json",
        ".codex/validation/selection-request-v1.schema.json",
        ".codex/validation/test-replacements-v2.schema.json",
    ];
    let schemas = schema_paths
        .into_iter()
        .map(|path| {
            let schema: Value = serde_json::from_slice(schema_bytes(path)).expect("schema parses");
            let id = schema["$id"]
                .as_str()
                .expect("schema claims an ID")
                .to_owned();
            (id, schema)
        })
        .collect::<Vec<_>>();
    let mut registry = jsonschema::Registry::new();
    for (id, schema) in &schemas {
        registry = registry
            .add(id.as_str(), schema)
            .expect("schema resource registers");
    }
    let registry = registry.prepare().expect("schema registry prepares");
    let validate = |schema_id: &str, instance: &Value| {
        let schema = schemas
            .iter()
            .find(|(id, _)| id == schema_id)
            .map(|(_, schema)| schema)
            .expect("schema ID is registered");
        let validator = jsonschema::options()
            .with_registry(&registry)
            .build(schema)
            .expect("Draft 2020-12 schema compiles");
        if let Err(error) = validator.validate(instance) {
            panic!("{schema_id} rejected serialized Rust value: {error}");
        }
    };

    let fixture = shared_fixture_value();
    let request: SelectionRequestV1 =
        serde_json::from_value(fixture["selection_request_vectors"]["valid"][0]["request"].clone())
            .expect("selection request parses");
    validate(
        "kd4://validation/selection-request-v1.schema.json",
        &serde_json::to_value(request).expect("selection request serializes"),
    );
    let ledger: TestReplacementLedgerV2 =
        serde_json::from_value(fixture["replacement_ledger_vectors"][1].clone())
            .expect("replacement ledger parses");
    validate(
        "kd4://validation/test-replacements-v2.schema.json",
        &serde_json::to_value(ledger).expect("replacement ledger serializes"),
    );
    let inventory_entry: ExecutableInventoryEntryV2 = serde_json::from_value(
        fixture["selection_contract_vectors"][0]["resolved_entries"][0]["inventory_entry"].clone(),
    )
    .expect("inventory entry parses");
    let inventory_entry_wrapper = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "kd4://validation/frozen-test-inventory-v2.schema.json#/$defs/entry",
    });
    jsonschema::options()
        .with_registry(&registry)
        .build(&inventory_entry_wrapper)
        .expect("inventory entry wrapper compiles")
        .validate(&serde_json::to_value(inventory_entry).expect("inventory entry serializes"))
        .expect("inventory schema validates serialized inventory entry");

    let provenance: ProvenanceReceiptV1 =
        serde_json::from_value(fixture["provenance_receipt_vectors"][0].clone())
            .expect("provenance parses");
    let shared_wrapper = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "kd4://validation/inventory-shared-types-v1.schema.json#/$defs/provenance_receipt",
    });
    let shared_validator = jsonschema::options()
        .with_registry(&registry)
        .build(&shared_wrapper)
        .expect("shared type wrapper compiles");
    shared_validator
        .validate(&serde_json::to_value(provenance).expect("provenance serializes"))
        .expect("shared schema validates serialized provenance");

    let hash = |digit: char| {
        Sha256HexV1::parse(std::iter::repeat_n(digit, 64).collect::<String>())
            .expect("fixture hash")
    };
    let mut transition = RecoveryTransitionReceiptV1 {
        authority_before_semantic_sha256: hash('1'),
        child_obligation_ids: vec!["inventory-v2-recovered.child".to_owned()],
        frozen_source_authority_sha256: hash('2'),
        parent_container_ids: vec!["parent-000".to_owned()],
        recapture_receipt_sha256: hash('3'),
        receipt_sha256: hash('0'),
        schema_version: 1,
    };
    let mut transition_projection =
        serde_json::to_value(&transition).expect("transition serializes");
    transition_projection
        .as_object_mut()
        .expect("transition is an object")
        .remove("receipt_sha256");
    transition.receipt_sha256 = proof_hash(
        RecoveryTransitionReceiptV1::HASH_DOMAIN,
        &transition_projection,
    )
    .expect("transition hashes");
    let recovery_wrapper = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "kd4://validation/frozen-test-inventory-v2-recoveries.schema.json#/$defs/transition_receipt",
    });
    let recovery_validator = jsonschema::options()
        .with_registry(&registry)
        .build(&recovery_wrapper)
        .expect("recovery type wrapper compiles");
    recovery_validator
        .validate(&serde_json::to_value(transition).expect("transition serializes"))
        .expect("recovery schema validates serialized transition receipt");

    let (authority_value, _) = active_host_authority_value();
    let authority: ActiveHostApplicabilityAuthorityV1 =
        serde_json::from_value(authority_value).expect("authority parses");
    let authority_wrapper = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "kd4://validation/inventory-shared-types-v1.schema.json#/$defs/active_host_applicability_authority",
    });
    jsonschema::options()
        .with_registry(&registry)
        .build(&authority_wrapper)
        .expect("authority type wrapper compiles")
        .validate(&serde_json::to_value(authority).expect("authority serializes"))
        .expect("shared schema validates serialized authority");
}

#[test]
fn inventory_enforces_frozen_predecessor_schema_set_and_distinct_action_inputs() {
    let inventory = valid_inventory_value();
    assert!(
        serde_json::from_value::<FrozenTestInventoryV2>(inventory.clone())
            .expect("inventory parses")
            .validate()
            .is_err(),
        "an inventory without all 15,544 explicit frozen baseline declarations must fail"
    );

    let mut wrong_predecessor = inventory.clone();
    wrong_predecessor["predecessor"]["test_count"] = json!(15_543);
    refresh_inventory_hashes(&mut wrong_predecessor);
    assert!(
        serde_json::from_value::<FrozenTestInventoryV2>(wrong_predecessor)
            .expect("inventory parses")
            .validate()
            .is_err(),
        "the immutable V1 predecessor count must remain 15,544"
    );

    let mut incomplete_schema_set = inventory.clone();
    incomplete_schema_set["schema_resources"]
        .as_array_mut()
        .expect("schema resources array")
        .pop();
    refresh_inventory_hashes(&mut incomplete_schema_set);
    assert!(
        serde_json::from_value::<FrozenTestInventoryV2>(incomplete_schema_set)
            .expect("inventory parses")
            .validate()
            .is_err(),
        "all five dormant schema resources are required"
    );

    let mut shared_action_input = inventory;
    let first_hash =
        shared_action_input["action_routes"][0]["execution_input_contract_sha256"].clone();
    shared_action_input["action_routes"][1]["execution_input_contract_sha256"] = first_hash;
    refresh_inventory_hashes(&mut shared_action_input);
    assert!(
        serde_json::from_value::<FrozenTestInventoryV2>(shared_action_input)
            .expect("inventory parses")
            .validate()
            .is_err(),
        "the two action routes must have distinct input contracts"
    );
}

#[test]
fn predecessor_projection_reconciles_every_frozen_inventory_and_ledger_id() {
    let frozen_inventory: Value = serde_json::from_slice(include_bytes!(
        "../../../.codex/validation/frozen-test-inventory-v1.json"
    ))
    .expect("frozen V1 inventory parses");
    let frozen_ledger: Value = serde_json::from_slice(include_bytes!(
        "../../../.codex/validation/test-replacements-v1.json"
    ))
    .expect("frozen V1 ledger parses");
    let mut inventory_ids = frozen_inventory["tests"]
        .as_array()
        .expect("tests array")
        .iter()
        .map(|row| row["baseline_id"].as_str().expect("baseline ID").to_owned())
        .collect::<Vec<_>>();
    let mut ledger_ids = frozen_ledger["rows"]
        .as_array()
        .expect("rows array")
        .iter()
        .map(|row| row["baseline_id"].as_str().expect("baseline ID").to_owned())
        .collect::<Vec<_>>();
    inventory_ids.sort();
    ledger_ids.sort();
    assert_eq!(inventory_ids, ledger_ids);
    let mut associations = frozen_inventory["tests"]
        .as_array()
        .expect("tests array")
        .iter()
        .map(|row| FrozenBaselineAssociationV1 {
            baseline_id: row["baseline_id"].as_str().expect("baseline ID").to_owned(),
            predecessor_entry_sha256: proof_hash("kd4.frozen-v1-inventory-entry.v1", row)
                .expect("predecessor inventory entry hashes"),
        })
        .collect::<Vec<_>>();
    associations.sort_by(|left, right| left.baseline_id.cmp(&right.baseline_id));
    let projection = json!({
        "frozen_baseline_associations": associations,
        "frozen_baseline_associations_sha256": PredecessorArtifactReconciliationV1::FROZEN_BASELINE_ASSOCIATIONS_SHA256,
        "frozen_baseline_ids": inventory_ids,
        "frozen_baseline_ids_sha256": PredecessorArtifactReconciliationV1::FROZEN_BASELINE_IDS_SHA256,
        "frozen_inventory_raw_sha256": PredecessorArtifactReconciliationV1::FROZEN_INVENTORY_RAW_SHA256,
        "frozen_inventory_semantic_sha256": PredecessorArtifactReconciliationV1::FROZEN_INVENTORY_SEMANTIC_SHA256,
        "frozen_ledger_raw_sha256": PredecessorArtifactReconciliationV1::FROZEN_LEDGER_RAW_SHA256,
        "schema_version": 1,
    });
    let mut value = projection.clone();
    value["projection_sha256"] = Value::String(
        proof_hash(
            PredecessorArtifactReconciliationV1::HASH_DOMAIN,
            &projection,
        )
        .expect("projection hashes")
        .to_string(),
    );
    let reconciliation: PredecessorArtifactReconciliationV1 =
        serde_json::from_value(value).expect("reconciliation parses");
    reconciliation.validate().expect("reconciliation validates");
    reconciliation
        .validate_observed_baseline_ids(&ledger_ids, "fixture ledger IDs")
        .expect("ledger IDs reconcile");

    let mut substituted = serde_json::to_value(&reconciliation).expect("reconciliation serializes");
    let rows = substituted["frozen_baseline_associations"]
        .as_array_mut()
        .expect("association rows");
    let first_hash = rows[0]["predecessor_entry_sha256"].clone();
    rows[0]["predecessor_entry_sha256"] = rows[1]["predecessor_entry_sha256"].clone();
    rows[1]["predecessor_entry_sha256"] = first_hash;
    let rows = substituted["frozen_baseline_associations"].clone();
    substituted["frozen_baseline_associations_sha256"] = Value::String(
        proof_hash("kd4.frozen-baseline-association-set.v1", &rows)
            .expect("substituted associations hash")
            .to_string(),
    );
    let mut projection = substituted.clone();
    projection
        .as_object_mut()
        .expect("reconciliation object")
        .remove("projection_sha256");
    substituted["projection_sha256"] = Value::String(
        proof_hash(
            PredecessorArtifactReconciliationV1::HASH_DOMAIN,
            &projection,
        )
        .expect("substituted reconciliation hashes")
        .to_string(),
    );
    let substituted: PredecessorArtifactReconciliationV1 =
        serde_json::from_value(substituted).expect("substituted reconciliation parses");
    assert!(
        substituted.validate().is_err(),
        "the frozen association anchor prevents predecessor entry permutation"
    );
}

#[test]
fn exact_schema_resources_and_recovery_contracts_match_python() {
    let paths = [
        ".codex/validation/frozen-test-inventory-v2-recoveries.schema.json",
        ".codex/validation/frozen-test-inventory-v2.schema.json",
        ".codex/validation/inventory-shared-types-v1.schema.json",
        ".codex/validation/selection-request-v1.schema.json",
        ".codex/validation/test-replacements-v2.schema.json",
    ];
    let resources = paths
        .into_iter()
        .map(|path| SchemaResourceRefV1 {
            path: StrictRepositoryPathV1::parse(path.to_owned()).expect("schema path"),
            raw_sha256: Sha256HexV1::parse(format!(
                "{:x}",
                sha2::Sha256::digest(schema_bytes(path))
            ))
            .expect("schema raw hash"),
            schema_id: serde_json::from_slice::<Value>(schema_bytes(path))
                .expect("schema parses")
                .get("$id")
                .and_then(Value::as_str)
                .expect("schema claims an ID")
                .to_owned(),
        })
        .collect::<Vec<_>>();
    let resources_sha256 = proof_hash(
        FrozenTestInventoryV2::SCHEMA_RESOURCE_SET_HASH_DOMAIN,
        &resources,
    )
    .expect("schema resource set hashes");
    FrozenTestInventoryV2::validate_schema_resources(&resources, &resources_sha256)
        .expect("exact schema resources validate");
    let mut substituted = resources.clone();
    substituted[0].raw_sha256 = Sha256HexV1::parse("0".repeat(64)).expect("fixture hash");
    assert!(
        FrozenTestInventoryV2::validate_schema_resources(&substituted, &resources_sha256).is_err()
    );
    let mut substituted = resources.clone();
    substituted[0].schema_id = "kd4://validation/substitute.schema.json".to_owned();
    assert!(
        FrozenTestInventoryV2::validate_schema_resources(&substituted, &resources_sha256).is_err()
    );

    let parameter = CanonicalParameterProjectionV1::Mapping {
        entries: vec![
            codex_validation_contracts::recovery::CanonicalParameterMappingEntryV1 {
                key: CanonicalParameterProjectionV1::String {
                    value: "case".to_owned(),
                },
                value: CanonicalParameterProjectionV1::Tuple {
                    items: vec![CanonicalParameterProjectionV1::Bytes {
                        base64url: "AP8".to_owned(),
                    }],
                },
            },
        ],
    };
    parameter.validate().expect("canonical parameters validate");
    assert!(
        CanonicalParameterProjectionV1::Bytes {
            base64url: "AP8=".to_owned()
        }
        .validate()
        .is_err()
    );

    let hash = |digit: char| {
        Sha256HexV1::parse(std::iter::repeat_n(digit, 64).collect::<String>())
            .expect("fixture hash")
    };
    let mut transition = RecoveryTransitionReceiptV1 {
        authority_before_semantic_sha256: hash('1'),
        child_obligation_ids: vec!["inventory-v2-recovered.child".to_owned()],
        frozen_source_authority_sha256: hash('2'),
        parent_container_ids: vec!["parent-000".to_owned()],
        recapture_receipt_sha256: hash('3'),
        receipt_sha256: hash('0'),
        schema_version: 1,
    };
    let mut projection = serde_json::to_value(&transition).expect("transition serializes");
    projection
        .as_object_mut()
        .expect("transition object")
        .remove("receipt_sha256");
    transition.receipt_sha256 = proof_hash(RecoveryTransitionReceiptV1::HASH_DOMAIN, &projection)
        .expect("transition hashes");
    transition.validate().expect("transition validates");
}

fn valid_inventory_value() -> Value {
    let contract = |path: &str| {
        let projection = json!({
            "consumed": [],
            "owned": [{"kind": "exact", "path": path}],
            "schema_version": 1,
        });
        let digest = proof_hash("kd4.execution-input-contract.v1", &projection)
            .expect("contract hashes")
            .to_string();
        json!({
            "consumed": [],
            "contract_id": format!("execution-input-contract-v1.{digest}"),
            "contract_sha256": digest,
            "owned": [{"kind": "exact", "path": path}],
            "schema_version": 1,
        })
    };
    let mut contracts = vec![contract("SOURCEMAP.md"), contract("docs/README.md")];
    contracts.sort_by_key(|value| canonical_jcs(value).expect("contract canonicalizes"));
    let mut action_routes = vec![
        json!({
            "action_id": "documentation.markdown",
            "execution_input_contract_sha256": contracts[0]["contract_sha256"],
            "validation_id": "documentation.markdown",
        }),
        json!({
            "action_id": "maintenance.source-map",
            "execution_input_contract_sha256": contracts[1]["contract_sha256"],
            "validation_id": "maintenance.source-map",
        }),
    ];
    action_routes.sort_by_key(|value| canonical_jcs(value).expect("route canonicalizes"));
    let route_kinds = [
        "argument-comment-lint-native",
        "javascript-jest",
        "python-pytest",
        "python-unittest",
        "rust-doctest",
        "rust-nextest",
        "windows-sandbox-smoke-native",
    ];
    let routes = route_kinds
        .into_iter()
        .map(|kind| {
            json!({
                "route_id": format!("test-route.{kind}.v1"),
                "runner_kind": kind,
                "validation_id": format!("validate.{kind}"),
            })
        })
        .collect::<Vec<_>>();
    let schema_resources = [
        ".codex/validation/frozen-test-inventory-v2-recoveries.schema.json",
        ".codex/validation/frozen-test-inventory-v2.schema.json",
        ".codex/validation/inventory-shared-types-v1.schema.json",
        ".codex/validation/selection-request-v1.schema.json",
        ".codex/validation/test-replacements-v2.schema.json",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, path)| {
        json!({
            "path": path,
            "raw_sha256": format!("{0:064x}", index + 1),
            "schema_id": format!(
                "kd4://validation/{}",
                path.rsplit('/').next().expect("schema file name")
            ),
        })
    })
    .collect::<Vec<_>>();
    let predecessor_reconciliation = json!({
        "frozen_baseline_associations": [],
        "frozen_baseline_associations_sha256": PredecessorArtifactReconciliationV1::FROZEN_BASELINE_ASSOCIATIONS_SHA256,
        "frozen_baseline_ids": [],
        "frozen_baseline_ids_sha256": PredecessorArtifactReconciliationV1::FROZEN_BASELINE_IDS_SHA256,
        "frozen_inventory_raw_sha256": PredecessorArtifactReconciliationV1::FROZEN_INVENTORY_RAW_SHA256,
        "frozen_inventory_semantic_sha256": PredecessorArtifactReconciliationV1::FROZEN_INVENTORY_SEMANTIC_SHA256,
        "frozen_ledger_raw_sha256": PredecessorArtifactReconciliationV1::FROZEN_LEDGER_RAW_SHA256,
        "projection_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
        "schema_version": 1,
    });
    let mut inventory = json!({
        "action_routes": action_routes,
        "authority": {
            "format_id": "kd4-frozen-test-inventory-v2",
            "raw_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "schema_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "semantic_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
            "self_hash": "0000000000000000000000000000000000000000000000000000000000000000",
        },
        "cargo_target_context_specs": [],
        "declaration_universe": [],
        "execution_input_contracts": contracts,
        "format_id": "kd4-frozen-test-inventory-v2",
        "predecessor": {
            "inventory_hash": PredecessorArtifactReconciliationV1::FROZEN_INVENTORY_SEMANTIC_SHA256,
            "raw_sha256": PredecessorArtifactReconciliationV1::FROZEN_INVENTORY_RAW_SHA256,
            "recorded_baseline_workspace_fingerprint": "7d5c019e4af3720e1188704b05a098e95fdb200b901cbccf5cf13363643f34ca",
            "test_count": 15_544,
        },
        "predecessor_reconciliation": predecessor_reconciliation,
        "recovery_authority": {
            "path": ".codex/validation/frozen-test-inventory-v2-recoveries.json",
            "raw_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
            "semantic_sha256": "2222222222222222222222222222222222222222222222222222222222222222",
            "self_hash": "3333333333333333333333333333333333333333333333333333333333333333",
        },
        "routes": routes,
        "schema_resources": schema_resources,
        "schema_resources_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
        "schema_version": 2,
    });
    refresh_inventory_hashes(&mut inventory);
    inventory
}

fn refresh_inventory_hashes(inventory: &mut Value) {
    let semantic_projection = json!({
        "action_routes": inventory["action_routes"],
        "cargo_target_context_specs": inventory["cargo_target_context_specs"],
        "declaration_universe": inventory["declaration_universe"],
        "execution_input_contracts": inventory["execution_input_contracts"],
        "format_id": inventory["format_id"],
        "predecessor": inventory["predecessor"],
        "predecessor_reconciliation": inventory["predecessor_reconciliation"],
        "recovery_authority": inventory["recovery_authority"],
        "routes": inventory["routes"],
        "schema_resources": inventory["schema_resources"],
        "schema_resources_sha256": inventory["schema_resources_sha256"],
        "schema_version": inventory["schema_version"],
    });
    inventory["authority"]["semantic_sha256"] = Value::String(
        proof_hash(
            FrozenTestInventoryV2::SEMANTIC_HASH_DOMAIN,
            &semantic_projection,
        )
        .expect("semantic projection hashes")
        .to_string(),
    );
    let authority_projection = json!({
        "format_id": inventory["authority"]["format_id"],
        "raw_sha256": inventory["authority"]["raw_sha256"],
        "schema_sha256": inventory["authority"]["schema_sha256"],
        "semantic_sha256": inventory["authority"]["semantic_sha256"],
    });
    inventory["authority"]["self_hash"] = Value::String(
        proof_hash(
            FrozenTestInventoryV2::AUTHORITY_SELF_HASH_DOMAIN,
            &authority_projection,
        )
        .expect("authority projection hashes")
        .to_string(),
    );
}

fn refresh_recovery_hashes(recovery: &mut Value) {
    let semantic_projection = json!({
        "format_id": recovery["format_id"],
        "frozen_source_authority": recovery["frozen_source_authority"],
        "records": recovery["records"],
        "schema_version": recovery["schema_version"],
    });
    recovery["semantic_sha256"] = Value::String(
        proof_hash(
            InventoryRecoveryAuthorityV1::SEMANTIC_HASH_DOMAIN,
            &semantic_projection,
        )
        .expect("recovery semantic projection hashes")
        .to_string(),
    );
    let self_projection = json!({
        "format_id": recovery["format_id"],
        "frozen_source_authority": recovery["frozen_source_authority"],
        "records": recovery["records"],
        "schema_version": recovery["schema_version"],
        "semantic_sha256": recovery["semantic_sha256"],
    });
    recovery["self_hash"] = Value::String(
        proof_hash(
            InventoryRecoveryAuthorityV1::SELF_HASH_DOMAIN,
            &self_projection,
        )
        .expect("recovery self projection hashes")
        .to_string(),
    );
}

fn refresh_transition_receipt_hash(receipt: &mut Value) {
    let projection = json!({
        "authority_before_semantic_sha256": receipt["authority_before_semantic_sha256"],
        "child_obligation_ids": receipt["child_obligation_ids"],
        "frozen_source_authority_sha256": receipt["frozen_source_authority_sha256"],
        "parent_container_ids": receipt["parent_container_ids"],
        "recapture_receipt_sha256": receipt["recapture_receipt_sha256"],
        "schema_version": receipt["schema_version"],
    });
    receipt["receipt_sha256"] = Value::String(
        proof_hash(RecoveryTransitionReceiptV1::HASH_DOMAIN, &projection)
            .expect("transition receipt projection hashes")
            .to_string(),
    );
}

fn sort_recovered_children(record: &mut Value) {
    record["resolution"]["child_sources"]
        .as_array_mut()
        .expect("recovered child sources are an array")
        .sort_by_key(|child| {
            serde_json::from_value::<RecoveredChildSourceV1>(child.clone())
                .expect("recovered child source parses")
                .obligation_id()
                .expect("recovered child obligation hashes")
        });
}

fn refresh_ledger_hashes(ledger: &mut Value) {
    let semantic_projection = json!({
        "format_id": ledger["format_id"],
        "inventory_authority": ledger["inventory_authority"],
        "rows": ledger["rows"],
        "schema_version": ledger["schema_version"],
        "trusted_defect_receipts": ledger["trusted_defect_receipts"],
    });
    ledger["semantic_sha256"] = Value::String(
        proof_hash(
            TestReplacementLedgerV2::SEMANTIC_HASH_DOMAIN,
            &semantic_projection,
        )
        .expect("ledger semantic projection hashes")
        .to_string(),
    );
    let self_projection = json!({
        "format_id": ledger["format_id"],
        "inventory_authority": ledger["inventory_authority"],
        "rows": ledger["rows"],
        "schema_version": ledger["schema_version"],
        "semantic_sha256": ledger["semantic_sha256"],
        "trusted_defect_receipts": ledger["trusted_defect_receipts"],
    });
    ledger["self_hash"] = Value::String(
        proof_hash(TestReplacementLedgerV2::SELF_HASH_DOMAIN, &self_projection)
            .expect("ledger self projection hashes")
            .to_string(),
    );
}

fn sort_ledger_rows(ledger: &mut Value) {
    ledger["rows"]
        .as_array_mut()
        .expect("ledger rows are an array")
        .sort_by(|left, right| {
            canonical_jcs(left)
                .expect("left ledger row canonicalizes")
                .cmp(&canonical_jcs(right).expect("right ledger row canonicalizes"))
        });
}

fn shared_fixture_value() -> Value {
    serde_json::from_slice(include_bytes!(
        "../../../scripts/fixtures/completion_proof_v2_cross_language_vectors.json"
    ))
    .expect("shared fixture parses")
}

fn active_host_authority_value() -> (Value, [u8; 32]) {
    let fixture = shared_fixture_value();
    let inventory_entry =
        &fixture["selection_contract_vectors"][0]["resolved_entries"][0]["inventory_entry"];
    let inventory_authority =
        fixture["selection_contract_vectors"][0]["inventory_authority"].clone();
    let mut applicability_result = json!({
        "cargo_build_context_observation_sha256": null,
        "cargo_target_context_spec_sha256": null,
        "executable_identity_sha256": inventory_entry["executable_identity_sha256"],
        "host": "windows",
        "platform_applicability_sha256": inventory_entry["platform_applicability_sha256"],
        "result_sha256": "0".repeat(64),
        "rust_cfg_expression_semantic_sha256": null,
        "schema_version": 1,
        "verdict": "applicable",
    });
    let mut result_projection = applicability_result.clone();
    result_projection
        .as_object_mut()
        .expect("applicability result is an object")
        .remove("result_sha256");
    applicability_result["result_sha256"] = json!(
        proof_hash("kd4.applicability-result.v1", &result_projection)
            .expect("applicability result hashes")
            .to_string()
    );
    let projection = json!({
        "entries": [{
            "applicability_result": applicability_result,
            "identity": inventory_entry["executable_identity"],
            "identity_sha256": inventory_entry["executable_identity_sha256"],
            "platform_applicability_sha256": inventory_entry["platform_applicability_sha256"],
        }],
        "host": "windows",
        "inventory_authority": inventory_authority,
        "schema_version": 1,
    });
    let body = json!({
        "authority_nonce": STANDARD_NO_PAD.encode([b'N'; 32]),
        "inventory_authority": projection["inventory_authority"],
        "schema_version": 1,
        "target_applicability_projection": projection,
        "target_applicability_projection_sha256": proof_hash(
            "kd4.target-applicability-projection.v1",
            &projection,
        )
        .expect("target applicability projection hashes")
        .to_string(),
    });
    let authority_sha256 = proof_hash("kd4.active-host-applicability-authority.v1", &body)
        .expect("authority hashes")
        .to_string();
    let mut value = json!({
        "authority_sha256": authority_sha256,
        "body": body,
        "key_id": "12345678-1234-4234-8234-123456789abc",
        "schema_version": 1,
    });
    let authentication_key = [b'K'; 32];
    let mut mac =
        Hmac::<Sha256>::new_from_slice(&authentication_key).expect("32-byte HMAC key is valid");
    mac.update(b"kd4.active-host-applicability-authority.authentication.v1\0");
    mac.update(&canonical_jcs(&value).expect("authentication projection canonicalizes"));
    value["authentication_tag"] = json!(STANDARD_NO_PAD.encode(mac.finalize().into_bytes()));
    (value, authentication_key)
}

fn assert_closed_object_schemas(value: &Value, location: &str) {
    match value {
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("object") {
                assert_eq!(
                    object.get("additionalProperties").and_then(Value::as_bool),
                    Some(false),
                    "open object schema at {location}"
                );
            }
            for (key, nested) in object {
                assert_closed_object_schemas(nested, &format!("{location}/{key}"));
            }
        }
        Value::Array(values) => {
            for (index, nested) in values.iter().enumerate() {
                assert_closed_object_schemas(nested, &format!("{location}/{index}"));
            }
        }
        _ => {}
    }
}

fn schema_bytes(relative: &str) -> &'static [u8] {
    match relative {
        ".codex/validation/frozen-test-inventory-v2-recoveries.schema.json" => include_bytes!(
            "../../../.codex/validation/frozen-test-inventory-v2-recoveries.schema.json"
        ),
        ".codex/validation/frozen-test-inventory-v2.schema.json" => {
            include_bytes!("../../../.codex/validation/frozen-test-inventory-v2.schema.json")
        }
        ".codex/validation/inventory-shared-types-v1.schema.json" => {
            include_bytes!("../../../.codex/validation/inventory-shared-types-v1.schema.json")
        }
        ".codex/validation/selection-request-v1.schema.json" => {
            include_bytes!("../../../.codex/validation/selection-request-v1.schema.json")
        }
        ".codex/validation/test-replacements-v2.schema.json" => {
            include_bytes!("../../../.codex/validation/test-replacements-v2.schema.json")
        }
        other => panic!("fixture names an unknown schema: {other}"),
    }
}
