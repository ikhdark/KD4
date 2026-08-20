use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(crate) const READ_TURN_TIMING_TOOL_NAME: &str = "read_turn_timing";

pub(crate) fn create_read_turn_timing_tool() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: READ_TURN_TIMING_TOOL_NAME.to_string(),
        description: "Read the persisted terminal timing profile for one turn without parsing the thread transcript. By default returns a bounded audit summary with the exclusive wall-clock partition, overlapping activity unions, key counters, provider-token totals, and the five slowest model requests. Use detail=full only when the complete exact TurnTiming record is required. If turn_id is omitted, the latest terminal turn is selected."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([
                (
                    "thread_id".to_string(),
                    JsonSchema::string(Some(
                        "Thread UUID whose persisted timing should be read.".to_string(),
                    )),
                ),
                (
                    "turn_id".to_string(),
                    JsonSchema::string(Some(
                        "Optional turn ID. Omit to select the latest completed or aborted turn."
                            .to_string(),
                    )),
                ),
                (
                    "detail".to_string(),
                    JsonSchema::string_enum(
                        vec![
                            serde_json::Value::String("summary".to_string()),
                            serde_json::Value::String("full".to_string()),
                        ],
                        Some(
                            "summary is bounded and audit-oriented; full returns the exact stored timing profile. Defaults to summary."
                                .to_string(),
                        ),
                    ),
                ),
            ]),
            Some(vec!["thread_id".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_tool_defaults_to_a_bounded_summary_and_offers_full_fidelity() {
        let tool = serde_json::to_value(create_read_turn_timing_tool())
            .expect("serialize read_turn_timing spec");
        assert_eq!(tool["name"], READ_TURN_TIMING_TOOL_NAME);
        assert_eq!(
            tool.pointer("/parameters/required/0"),
            Some(&serde_json::json!("thread_id"))
        );
        assert_eq!(
            tool.pointer("/parameters/properties/detail/enum"),
            Some(&serde_json::json!(["summary", "full"]))
        );
        let description = tool["description"].as_str().expect("description");
        assert!(description.contains("five slowest model requests"));
        assert!(description.contains("complete exact TurnTiming"));
    }
}
