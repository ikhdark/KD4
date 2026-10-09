use super::canonicalize;
use super::config_schema_json;
use super::write_config_schema;

use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn trim_single_trailing_newline(contents: &str) -> &str {
    contents.strip_suffix('\n').unwrap_or(contents)
}

#[test]
fn config_schema_matches_fixture() {
    // Embedding the existing fixture makes changes invalidate this test binary,
    // without depending on the core crate or the caller's working directory.
    let fixture = include_str!("../../core/config.schema.json");
    let fixture_value: serde_json::Value =
        serde_json::from_str(fixture).expect("parse config schema fixture");
    let schema_json = config_schema_json().expect("serialize config schema");
    let schema_value: serde_json::Value =
        serde_json::from_slice(&schema_json).expect("decode schema json");
    let fixture_value = canonicalize(&fixture_value);
    let schema_value = canonicalize(&schema_value);
    assert_eq!(
        fixture_value, schema_value,
        "regenerate the config schema fixture with `just config-schema-regenerate <owner>`"
    );

    // Keep both the returned bytes and the public writer tied to the exact fixture.
    let fixture = fixture.replace("\r\n", "\n");
    let generated = String::from_utf8(schema_json).expect("schema JSON is UTF-8");
    assert_eq!(
        trim_single_trailing_newline(&fixture),
        trim_single_trailing_newline(&generated),
        "fixture should match exactly with generated schema"
    );
    let tmp = TempDir::new().expect("create temp dir");
    let path = tmp.path().join("config.schema.json");
    write_config_schema(&path).expect("write schema");
    assert_eq!(std::fs::read_to_string(path).expect("read schema"), generated);
}
#[test]
fn config_schema_hides_unsupported_inline_mcp_bearer_token() {
    let schema_json = config_schema_json().expect("serialize config schema");
    let schema_value: serde_json::Value =
        serde_json::from_slice(&schema_json).expect("decode schema json");
    let properties = schema_value
        .pointer("/definitions/RawMcpServerConfig/properties")
        .expect("RawMcpServerConfig properties should exist")
        .as_object()
        .expect("RawMcpServerConfig properties should be an object");

    assert_eq!(
        (
            properties.contains_key("bearer_token"),
            properties.contains_key("bearer_token_env_var"),
        ),
        (false, true),
    );
}

#[test]
fn config_schema_excludes_removed_code_mode_waiting_policy() {
    let schema_json = config_schema_json().expect("serialize config schema");
    let schema_value: serde_json::Value =
        serde_json::from_slice(&schema_json).expect("decode schema json");
    let properties = schema_value
        .pointer("/definitions/CodeModeConfigToml/properties")
        .expect("CodeModeConfigToml properties should exist")
        .as_object()
        .expect("CodeModeConfigToml properties should be an object");

    assert!(!properties.contains_key("waiting_policy"));
}
