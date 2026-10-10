use super::ToolDefinition;
use crate::JsonSchema;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "lookup_order".to_string(),
        description: "Look up an order".to_string(),
        input_schema: JsonSchema::object(
            BTreeMap::new(),
            /*required*/ None,
            /*additional_properties*/ None,
        ),
        output_schema: Some(serde_json::json!({
            "type": "object",
        }).into()),
        defer_loading: false,
    }
}

#[test]
fn renamed_overrides_name_only() {
    assert_eq!(
        tool_definition().renamed("mcp__orders__lookup_order".to_string()),
        ToolDefinition {
            name: "mcp__orders__lookup_order".to_string(),
            ..tool_definition()
        }
    );
}
