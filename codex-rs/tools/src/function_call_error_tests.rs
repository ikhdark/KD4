use super::*;

#[test]
fn diagnostic_serialization_retains_routing_fields_and_omits_internal_severity() {
    let diagnostic = ToolFailureDiagnostic::fatal(
        ToolFailureClass::InvalidPayload,
        "tool_search.invalid_payload",
        "tool_search received an unsupported payload",
    )
    .with_owner_hint("tool_search input conversion")
    .with_next_action("inspect the shared payload converter");

    let value = serde_json::to_value(&diagnostic).expect("diagnostic should serialize");

    assert_eq!(value["class"], "invalid_payload");
    assert_eq!(value["fingerprint"], "tool_search.invalid_payload");
    assert_eq!(value["retryable"], false);
    assert_eq!(value["owner_hint"], "tool_search input conversion");
    assert_eq!(value["next_action"], "inspect the shared payload converter");
    assert!(value.get("fatal").is_none());
}

#[test]
fn function_call_error_exposes_structured_failure_identity() {
    let error = FunctionCallError::Diagnostic(ToolFailureDiagnostic::model_visible(
        ToolFailureClass::ToolExecution,
        "registry.unsupported_tool:missing",
        "unsupported tool",
    ));

    assert_eq!(
        error.fingerprint(),
        Some("registry.unsupported_tool:missing")
    );
    assert!(!error.is_fatal());
}
