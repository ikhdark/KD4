use super::*;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchemaPrimitiveType;
use codex_tools::JsonSchemaType;
use codex_tools::ToolSpec;

#[test]
fn verify_local_schema_is_strict_and_narrowing_only() {
    let ToolSpec::Function(tool) =
        create_verify_local_tool(VerifyLocalToolOptions::with_verify_local_environment_id(
            /*include_environment_id*/ false,
        ))
    else {
        panic!("expected function tool");
    };

    assert_eq!(tool.name, VERIFY_LOCAL_TOOL_NAME);
    assert!(tool.strict);
    assert_eq!(
        tool.parameters.additional_properties,
        Some(AdditionalProperties::Boolean(false))
    );
    assert_eq!(
        tool.parameters.schema_type,
        Some(JsonSchemaType::Single(JsonSchemaPrimitiveType::Object))
    );

    let properties = tool.parameters.properties.expect("properties");
    let fields = properties.keys().map(String::as_str).collect::<Vec<_>>();
    let expected_required = [
        "mode",
        "changed",
        "staged",
        "scope_current",
        "no_cache",
        "json",
    ];
    let mut expected_fields = expected_required;
    expected_fields.sort();
    assert_eq!(fields, expected_fields);
    assert_eq!(
        tool.parameters.required,
        Some(expected_required.iter().map(ToString::to_string).collect())
    );
    assert!(!properties.contains_key("environment_id"));

    for forbidden in [
        "all_dirty",
        "allow_workspace",
        "related",
        "related_tests",
        "isolated",
        "baseline",
        "retry_flakes",
        "cache_readonly",
        "regen",
        "scope_start",
        "scope_add",
        "scope_reset",
    ] {
        assert!(
            !properties.contains_key(forbidden),
            "{forbidden} must stay CLI-only"
        );
    }
}

#[test]
fn verify_local_multi_environment_schema_includes_and_requires_environment_id() {
    let ToolSpec::Function(tool) =
        create_verify_local_tool(VerifyLocalToolOptions::with_verify_local_environment_id(
            /*include_environment_id*/ true,
        ))
    else {
        panic!("expected function tool");
    };

    let properties = tool.parameters.properties.expect("properties");
    let fields = properties.keys().map(String::as_str).collect::<Vec<_>>();
    let expected_required = [
        "mode",
        "changed",
        "staged",
        "scope_current",
        "no_cache",
        "json",
        "environment_id",
    ];
    let mut expected_fields = expected_required;
    expected_fields.sort();
    assert_eq!(fields, expected_fields);
    assert_eq!(
        tool.parameters.required,
        Some(expected_required.iter().map(ToString::to_string).collect())
    );
    assert!(properties.contains_key("environment_id"));
}
