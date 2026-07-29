use serde_json::Value;
use serde_json::json;

fn evidence_schema() -> Value {
    serde_json::from_str(include_str!(
        "../../../../docs/schemas/investigation-evidence-v1.schema.json"
    ))
    .expect("investigation evidence schema should be valid JSON")
}

fn kds_example() -> Value {
    json!({
        "evidenceMeta": {
            "schemaVersion": 1,
            "producer": "kds",
            "operation": "compact",
            "evidenceBearing": true,
            "payloadCompleteness": "complete",
            "truncated": false,
            "approximate": false,
            "limitations": [],
            "snapshot": null
        },
        "exitCode": 0,
        "omittedBytes": 0,
        "report": "diagnostic report"
    })
}

#[test]
fn evidence_meta_v1_accepts_provider_examples() {
    let schema = evidence_schema();
    let validator =
        jsonschema::validator_for(&schema).expect("investigation evidence schema should compile");
    let mut interrupted_kds = kds_example();
    interrupted_kds["evidenceMeta"]["payloadCompleteness"] = json!("unknown");
    interrupted_kds["evidenceMeta"]["truncated"] = json!(true);
    interrupted_kds["evidenceMeta"]["limitations"] =
        json!(["child process did not provide a normal exit code"]);
    interrupted_kds["omittedBytes"] = json!(17);
    let examples = [
        kds_example(),
        interrupted_kds,
        json!({
            "evidenceMeta": {
                "schemaVersion": 1,
                "producer": "kdwg",
                "operation": "check",
                "evidenceBearing": true,
                "payloadCompleteness": "partial",
                "truncated": true,
                "approximate": false,
                "limitations": ["findings capped at 20 of 24"],
                "snapshot": "diff-sha256"
            },
            "verdict": "GAPS_FOUND",
            "report": {
                "verdict": "GAPS_FOUND",
                "changed_files": ["src/lib.rs"],
                "findings": [],
                "limitations": []
            }
        }),
        json!({
            "evidenceMeta": {
                "schemaVersion": 1,
                "producer": "repo-atlas",
                "operation": "trace",
                "evidenceBearing": true,
                "payloadCompleteness": "partial",
                "truncated": false,
                "approximate": true,
                "limitations": [
                    "trace paths are approximate identifier-based evidence; confirm with exact source reads"
                ],
                "snapshot": "index-sha256"
            },
            "paths": []
        }),
    ];

    for example in examples {
        assert!(
            validator.is_valid(&example),
            "provider example should validate: {example}"
        );
    }
}

#[test]
fn evidence_meta_v1_rejects_invalid_contract_values() {
    let schema = evidence_schema();
    let validator =
        jsonschema::validator_for(&schema).expect("investigation evidence schema should compile");
    let mut unknown_version = kds_example();
    unknown_version["evidenceMeta"]["schemaVersion"] = json!(2);

    let mut missing_producer = kds_example();
    missing_producer["evidenceMeta"]
        .as_object_mut()
        .expect("evidenceMeta should be an object")
        .remove("producer");

    let mut invalid_completeness = kds_example();
    invalid_completeness["evidenceMeta"]["payloadCompleteness"] = json!("full");

    let mut non_string_limitation = kds_example();
    non_string_limitation["evidenceMeta"]["limitations"] = json!(["bounded", 7]);

    let mut truncated_complete = kds_example();
    truncated_complete["evidenceMeta"]["truncated"] = json!(true);

    let mut empty_limitation = kds_example();
    empty_limitation["evidenceMeta"]["limitations"] = json!([""]);

    let mut empty_snapshot = kds_example();
    empty_snapshot["evidenceMeta"]["snapshot"] = json!("");

    let mut blank_limitation = kds_example();
    blank_limitation["evidenceMeta"]["limitations"] = json!(["   "]);

    let mut blank_snapshot = kds_example();
    blank_snapshot["evidenceMeta"]["snapshot"] = json!("   ");

    for invalid in [
        unknown_version,
        missing_producer,
        invalid_completeness,
        non_string_limitation,
        truncated_complete,
        empty_limitation,
        empty_snapshot,
        blank_limitation,
        blank_snapshot,
    ] {
        assert!(
            !validator.is_valid(&invalid),
            "invalid evidence metadata should be rejected: {invalid}"
        );
    }
}
