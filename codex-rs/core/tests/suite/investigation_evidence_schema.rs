use serde_json::Value;
use serde_json::json;

fn evidence_schema() -> Value {
    serde_json::from_str(include_str!(
        "../../../../docs/schemas/investigation-evidence-v1.schema.json"
    ))
    .expect("investigation evidence schema should be valid JSON")
}

fn provider_example() -> Value {
    json!({
        "evidenceMeta": {
            "schemaVersion": 1,
            "producer": "diagnostic-provider",
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
    let mut interrupted_provider = provider_example();
    interrupted_provider["evidenceMeta"]["payloadCompleteness"] = json!("unknown");
    interrupted_provider["evidenceMeta"]["truncated"] = json!(true);
    interrupted_provider["evidenceMeta"]["limitations"] =
        json!(["child process did not provide a normal exit code"]);
    interrupted_provider["omittedBytes"] = json!(17);
    let examples = [
        provider_example(),
        interrupted_provider,
        json!({
            "evidenceMeta": {
                "schemaVersion": 1,
                "producer": "wiring-provider",
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
                "producer": "  vendor.example/repository:v1  ",
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
    let mut unknown_version = provider_example();
    unknown_version["evidenceMeta"]["schemaVersion"] = json!(2);

    let mut missing_producer = provider_example();
    missing_producer["evidenceMeta"]
        .as_object_mut()
        .expect("evidenceMeta should be an object")
        .remove("producer");

    let mut invalid_completeness = provider_example();
    invalid_completeness["evidenceMeta"]["payloadCompleteness"] = json!("full");

    let mut non_string_limitation = provider_example();
    non_string_limitation["evidenceMeta"]["limitations"] = json!(["bounded", 7]);

    let mut truncated_complete = provider_example();
    truncated_complete["evidenceMeta"]["truncated"] = json!(true);

    let mut missing_snapshot = provider_example();
    missing_snapshot["evidenceMeta"]
        .as_object_mut()
        .expect("evidenceMeta should be an object")
        .remove("snapshot");

    let mut empty_limitation = provider_example();
    empty_limitation["evidenceMeta"]["limitations"] = json!([""]);

    let mut empty_snapshot = provider_example();
    empty_snapshot["evidenceMeta"]["snapshot"] = json!("");

    let mut blank_limitation = provider_example();
    blank_limitation["evidenceMeta"]["limitations"] = json!(["   "]);

    let mut blank_snapshot = provider_example();
    blank_snapshot["evidenceMeta"]["snapshot"] = json!("   ");

    let mut empty_producer = provider_example();
    empty_producer["evidenceMeta"]["producer"] = json!("");

    let mut blank_producer = provider_example();
    blank_producer["evidenceMeta"]["producer"] = json!("   ");

    for invalid in [
        unknown_version,
        missing_producer,
        invalid_completeness,
        non_string_limitation,
        truncated_complete,
        missing_snapshot,
        empty_limitation,
        empty_snapshot,
        blank_limitation,
        blank_snapshot,
        empty_producer,
        blank_producer,
    ] {
        assert!(
            !validator.is_valid(&invalid),
            "invalid evidence metadata should be rejected: {invalid}"
        );
    }
}
