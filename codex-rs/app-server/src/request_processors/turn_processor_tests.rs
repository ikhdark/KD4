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
    let additional_context = HashMap::from([(source, additional_context_entry("value"))]);

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
