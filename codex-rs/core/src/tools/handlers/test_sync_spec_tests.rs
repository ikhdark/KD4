use super::*;
use codex_tools::JsonSchema;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn test_sync_tool_matches_expected_spec() {
    assert_eq!(
        create_test_sync_tool(),
        ToolSpec::Function(ResponsesApiTool {
            name: "test_sync_tool".to_string(),
            description: "Internal synchronization helper used by Codex integration tests."
                .to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                    (
                        "barrier".to_string(),
                        JsonSchema::object(
                            BTreeMap::from([
                                (
                                    "id".to_string(),
                                    JsonSchema::string(Some(
                                        "Identifier shared by concurrent calls that should rendezvous"
                                            .to_string(),
                                    )),
                                ),
                                (
                                    "participants".to_string(),
                                    JsonSchema { minimum: Some(1.into()), ..JsonSchema::integer(Some(
                                        "Number of tool calls that must arrive before the barrier opens. Must be greater than zero."
                                            .to_string(),
                                    )) },
                                ),
                                (
                                    "timeout_ms".to_string(),
                                    JsonSchema { minimum: Some(1.into()), ..JsonSchema::integer(Some(
                                        "Maximum barrier wait in milliseconds. Must be greater than zero. Defaults to 1000."
                                            .to_string(),
                                    )) },
                                ),
                            ]),
                            Some(vec!["id".to_string(), "participants".to_string()]),
                            Some(false.into()),
                        ),
                    ),
                    (
                        "sleep_after_ms".to_string(),
                        JsonSchema { minimum: Some(0.into()), ..JsonSchema::integer(Some(
                            "Delay after completing the barrier. Defaults to no delay."
                                .to_string(),
                        )) },
                    ),
                    (
                        "sleep_before_ms".to_string(),
                        JsonSchema { minimum: Some(0.into()), ..JsonSchema::integer(Some(
                            "Delay before any other action. Defaults to no delay.".to_string(),
                        )) },
                    ),
                ]), /*required*/ None, Some(false.into())),
            output_schema: None,
        })
    );
}

#[test]
fn synchronization_schema_enforces_documented_numeric_domains() {
    let ToolSpec::Function(tool) = create_test_sync_tool() else {
        panic!("expected function tool");
    };
    let schema = serde_json::to_value(tool.parameters).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    // A rendezvous needs at least one participant and a positive timeout;
    // delays, unlike either barrier field, may express no delay with zero.
    for value in [
        serde_json::json!({}),
        serde_json::json!({"sleep_before_ms": 0, "sleep_after_ms": 0}),
        serde_json::json!({"barrier": {"id": "b", "participants": 1}}),
        serde_json::json!({"barrier": {"id": "b", "participants": 1, "timeout_ms": 1}}),
    ] {
        assert!(validator.is_valid(&value), "{value}");
    }
    for field in ["participants", "timeout_ms"] {
        for invalid in [serde_json::json!(-1), serde_json::json!(0), serde_json::json!(1.5)] {
            let mut value = serde_json::json!({"barrier": {"id": "b", "participants": 1, "timeout_ms": 1}});
            value["barrier"][field] = invalid;
            assert!(!validator.is_valid(&value), "{value}");
        }
    }
    for field in ["sleep_before_ms", "sleep_after_ms"] {
        let mut value = serde_json::json!({});
        value[field] = serde_json::json!(-1);
        assert!(!validator.is_valid(&value), "{value}");
    }
}
