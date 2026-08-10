use super::*;

fn additional_context_entry(value: impl Into<String>) -> AdditionalContextEntry {
    AdditionalContextEntry {
        value: value.into(),
        kind: AdditionalContextKind::Untrusted,
    }
}

#[test]
fn map_additional_context_rejects_oversized_source_identifier() {
    let source = "s".repeat(MAX_ADDITIONAL_CONTEXT_SOURCE_BYTES + 1);
    let additional_context = IndexMap::from([(source, additional_context_entry("value"))]);

    let error = map_additional_context(Some(additional_context))
        .expect_err("oversized additional-context source should be rejected");

    assert_eq!(error.code, -32600);
    assert_eq!(
        error.message,
        format!(
            "additionalContext source identifiers may contain at most {MAX_ADDITIONAL_CONTEXT_SOURCE_BYTES} bytes (longest was {} bytes)",
            MAX_ADDITIONAL_CONTEXT_SOURCE_BYTES + 1
        )
    );
}

#[test]
fn map_additional_context_rejects_too_many_entries() {
    let additional_context = (0..=MAX_ADDITIONAL_CONTEXT_ENTRIES)
        .map(|index| (format!("source-{index}"), additional_context_entry("value")))
        .collect();

    let error = map_additional_context(Some(additional_context))
        .expect_err("excess additional-context entries should be rejected");

    assert_eq!(error.code, -32600);
    assert_eq!(
        error.message,
        format!(
            "additionalContext may contain at most {MAX_ADDITIONAL_CONTEXT_ENTRIES} entries (received {})",
            MAX_ADDITIONAL_CONTEXT_ENTRIES + 1
        )
    );
}

#[test]
fn map_additional_context_rejects_aggregate_rendered_size() {
    let value = "v".repeat(MAX_ADDITIONAL_CONTEXT_VALUE_RENDERED_BYTES);
    let entry_count = MAX_ADDITIONAL_CONTEXT_AGGREGATE_RENDERED_BYTES
        / (MAX_ADDITIONAL_CONTEXT_VALUE_RENDERED_BYTES
            + ESTIMATED_ADDITIONAL_CONTEXT_WRAPPER_BYTES)
        + 1;
    assert!(entry_count <= MAX_ADDITIONAL_CONTEXT_ENTRIES);
    let additional_context = (0..entry_count)
        .map(|index| {
            (
                format!("source-{index}"),
                additional_context_entry(value.clone()),
            )
        })
        .collect();

    let error = map_additional_context(Some(additional_context))
        .expect_err("aggregate additional-context size should be rejected");

    assert_eq!(error.code, -32600);
    assert!(
        error.message.starts_with(&format!(
            "additionalContext may render to at most {MAX_ADDITIONAL_CONTEXT_AGGREGATE_RENDERED_BYTES} bytes"
        )),
        "unexpected error: {}",
        error.message
    );
}

#[test]
fn map_additional_context_preserves_client_order() {
    let additional_context = IndexMap::from([
        ("dependency".to_string(), additional_context_entry("first")),
        ("consumer".to_string(), additional_context_entry("second")),
    ]);

    let mapped = map_additional_context(Some(additional_context)).expect("context should map");

    assert_eq!(
        mapped.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["dependency", "consumer"]
    );
}

#[test]
fn bug_classifier_accepts_exact_multibyte_evidence_offsets() {
    let raw = "Crash in caf\u{e9}";
    let output = r#"{
        "summary":"A crash is reported.",
        "severity":null,
        "failureMechanism":{"value":"Crash","evidence":{"startByte":0,"endByte":5,"text":"Crash"}},
        "affectedComponents":[{"value":"café","evidence":{"startByte":9,"endByte":14,"text":"café"}}],
        "statedCause":null,
        "requiredRepair":null
    }"#;

    let result = parse_bug_classification(output, raw).expect("valid UTF-8 byte ranges");

    assert_eq!(result.failure_mechanism.as_deref(), Some("Crash"));
    assert_eq!(result.affected_components_json, r#"["café"]"#);
}

#[test]
fn bug_classifier_rejects_non_boundary_and_unsupported_facts() {
    let raw = "café";
    let non_boundary = r#"{
        "summary":"A report.",
        "severity":null,
        "failureMechanism":{"value":"é","evidence":{"startByte":4,"endByte":5,"text":"é"}},
        "affectedComponents":[],
        "statedCause":null,
        "requiredRepair":null
    }"#;
    let unsupported = r#"{
        "summary":"A report.",
        "severity":null,
        "failureMechanism":{"value":"invented","evidence":{"startByte":0,"endByte":3,"text":"caf"}},
        "affectedComponents":[],
        "statedCause":null,
        "requiredRepair":null
    }"#;

    assert!(matches!(
        parse_bug_classification(non_boundary, raw),
        Err(BugClassificationFailure::Grounding)
    ));
    assert!(matches!(
        parse_bug_classification(unsupported, raw),
        Err(BugClassificationFailure::Grounding)
    ));
}

#[test]
fn bug_classifier_requires_exact_schema_and_normalizes_cited_severity() {
    let raw = "HIGH failure";
    let valid = r#"{
        "summary":"A high-severity failure is reported.",
        "severity":{"value":"high","evidence":{"startByte":0,"endByte":4,"text":"HIGH"}},
        "failureMechanism":null,
        "affectedComponents":[],
        "statedCause":null,
        "requiredRepair":null
    }"#;
    let missing_key = r#"{
        "summary":"A report.",
        "severity":null,
        "failureMechanism":null,
        "affectedComponents":[],
        "statedCause":null
    }"#;
    let unknown_key = r#"{
        "summary":"A report.",
        "severity":null,
        "failureMechanism":null,
        "affectedComponents":[],
        "statedCause":null,
        "requiredRepair":null,
        "extra":"not allowed"
    }"#;

    let result = parse_bug_classification(valid, raw).expect("cited severity should normalize");
    assert_eq!(result.severity.as_deref(), Some("high"));
    assert!(matches!(
        parse_bug_classification(missing_key, raw),
        Err(BugClassificationFailure::Schema)
    ));
    assert!(matches!(
        parse_bug_classification(unknown_key, raw),
        Err(BugClassificationFailure::Schema)
    ));
}
