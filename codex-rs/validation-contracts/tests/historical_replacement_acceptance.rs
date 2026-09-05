use codex_validation_contracts::canonical::ContractError;
use codex_validation_contracts::canonical::Sha256HexV1;
use codex_validation_contracts::canonical::canonical_jcs;
use codex_validation_contracts::focused_replacement_approval::FocusedReplacementApprovalReceiptV1;
use codex_validation_contracts::historical_replacement_acceptance::HistoricalReplacementAcceptanceProposalV1;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root canonicalizes")
}

fn predecessor_ledger() -> Value {
    serde_json::from_slice(include_bytes!(
        "../../../.codex/validation/test-replacements-v1.json"
    ))
    .expect("frozen predecessor replacement ledger parses")
}

fn approval_receipts() -> Vec<FocusedReplacementApprovalReceiptV1> {
    let vectors: Value = serde_json::from_slice(include_bytes!(
        "../../../scripts/fixtures/focused_replacement_approval_receipt_v1_vectors.json"
    ))
    .expect("focused replacement approval vectors parse");
    vectors["valid_vectors"]
        .as_array()
        .expect("valid vectors are an array")
        .iter()
        .map(|vector| {
            serde_json::from_value(vector["receipt"].clone())
                .expect("valid focused replacement approval receipt parses")
        })
        .collect()
}

fn python_compiled_proposal() -> Vec<u8> {
    let python = std::env::var_os("PYTHON").unwrap_or_else(|| {
        if cfg!(windows) {
            "python".into()
        } else {
            "python3".into()
        }
    });
    let script = r#"
import json
import sys
from pathlib import Path
from scripts import replacement_admission as admission

root = Path(sys.argv[1])
plan = admission.compile_historical_replacement_review_plan(root)
vectors = json.loads(
    (root / "scripts/fixtures/focused_replacement_approval_receipt_v1_vectors.json")
    .read_text(encoding="utf-8")
)
receipt = vectors["valid_vectors"][0]["receipt"]
reviews = sorted(
    [
        {
            "review_scope_id": scope["review_scope_id"],
            "review_scope_sha256": scope["review_scope_sha256"],
            "disposition": admission.HISTORICAL_SCOPE_REVIEW_DISPOSITION,
        }
        for scope in plan["review_scopes"]
    ],
    key=lambda review: review["review_scope_id"],
)
proposal = admission.build_historical_replacement_acceptance_proposal_v1(
    plan, receipt, reviews
)
sys.stdout.buffer.write(admission.canonical_json(proposal))
"#;
    let root = repository_root();
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(&root)
        .current_dir(&root)
        .output()
        .expect("Python historical proposal compiler starts");
    assert!(
        output.status.success(),
        "Python historical proposal compiler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn python_compiler_proposal_is_accepted_by_rust_contract() {
    let predecessor = predecessor_ledger();
    let approval = approval_receipts().remove(0);
    let canonical = python_compiled_proposal();
    let proposal = HistoricalReplacementAcceptanceProposalV1::parse_canonical(
        &canonical,
        &predecessor,
        &approval,
    )
    .expect("Python historical acceptance proposal validates in Rust");

    assert_eq!(
        (
            proposal.baseline_count,
            proposal.edge_count,
            proposal.successor_count,
            proposal.review_scope_count,
            proposal.scope_reviews.len(),
        ),
        (644, 685, 572, 531, 531)
    );
    assert_eq!(
        proposal.frozen_graph_sha256.as_str(),
        HistoricalReplacementAcceptanceProposalV1::FROZEN_GRAPH_SHA256
    );
    assert_eq!(
        proposal
            .proposal_sha256()
            .expect("proposal digest computes"),
        proposal.proposal_sha256
    );
    assert_eq!(
        serde_json::to_value(proposal.activation_authority).expect("null authority serializes"),
        Value::Null
    );
}

#[test]
fn closed_contract_rejects_shape_hash_graph_scope_and_receipt_drift() {
    let predecessor = predecessor_ledger();
    let mut approvals = approval_receipts();
    let approval = approvals.remove(0);
    let canonical = python_compiled_proposal();
    let proposal = HistoricalReplacementAcceptanceProposalV1::parse_canonical(
        &canonical,
        &predecessor,
        &approval,
    )
    .expect("fixture proposal validates");

    let pretty = serde_json::to_vec_pretty(&proposal).expect("proposal pretty-serializes");
    assert_eq!(
        HistoricalReplacementAcceptanceProposalV1::parse_canonical(
            &pretty,
            &predecessor,
            &approval
        ),
        Err(ContractError::NonCanonicalJson)
    );

    let mut unknown: Value = serde_json::from_slice(&canonical).expect("proposal value parses");
    unknown
        .as_object_mut()
        .expect("proposal is an object")
        .insert("unexpected".to_owned(), Value::Bool(true));
    let unknown = canonical_jcs(&unknown).expect("unknown-field value canonicalizes");
    assert!(matches!(
        HistoricalReplacementAcceptanceProposalV1::parse_canonical(
            &unknown,
            &predecessor,
            &approval
        ),
        Err(ContractError::InvalidJson(_))
    ));

    let mut non_null_authority: Value =
        serde_json::from_slice(&canonical).expect("proposal value parses");
    non_null_authority["activation_authority"] = Value::Bool(true);
    assert!(
        serde_json::from_value::<HistoricalReplacementAcceptanceProposalV1>(non_null_authority)
            .is_err()
    );

    let mut wrong_hash = proposal.clone();
    wrong_hash.proposal_sha256 = Sha256HexV1::parse("0".repeat(64)).expect("test hash parses");
    assert!(wrong_hash.validate(&predecessor, &approval).is_err());

    let mut wrong_plan = proposal.clone();
    wrong_plan.review_plan_sha256 = Sha256HexV1::parse("0".repeat(64)).expect("test hash parses");
    assert!(wrong_plan.validate(&predecessor, &approval).is_err());

    let mut missing_scope = proposal.clone();
    missing_scope.scope_reviews.pop();
    assert!(missing_scope.validate(&predecessor, &approval).is_err());

    let mut rewired_scope = proposal.clone();
    rewired_scope.scope_reviews[0].review_scope_sha256 =
        rewired_scope.scope_reviews[1].review_scope_sha256.clone();
    assert!(rewired_scope.validate(&predecessor, &approval).is_err());

    let mut replayed_scope = proposal.clone();
    replayed_scope.scope_reviews[1] = replayed_scope.scope_reviews[0].clone();
    assert!(replayed_scope.validate(&predecessor, &approval).is_err());

    let mut altered_predecessor = predecessor.clone();
    let row = altered_predecessor["rows"]
        .as_array_mut()
        .expect("predecessor rows are an array")
        .iter_mut()
        .find(|row| row["resolution"] == "replacement")
        .expect("predecessor has a replacement row");
    row["replacement_ids"]
        .as_array_mut()
        .expect("replacement IDs are an array")[0] =
        Value::String("historical-successor:altered".to_owned());
    assert!(proposal.validate(&altered_predecessor, &approval).is_err());

    let different_approval = approvals.remove(0);
    different_approval
        .validate()
        .expect("second approval vector is intrinsically valid");
    assert!(
        proposal
            .validate(&predecessor, &different_approval)
            .is_err()
    );

    let mut invalid_approval = approval.clone();
    invalid_approval.receipt_sha256 = Sha256HexV1::parse("0".repeat(64)).expect("test hash parses");
    assert!(proposal.validate(&predecessor, &invalid_approval).is_err());
}
