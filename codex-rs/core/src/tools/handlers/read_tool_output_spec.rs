use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Number;
use std::collections::BTreeMap;

use crate::tools::command_output_artifact::ARTIFACT_SEARCH_MAX_CONTEXT_LINES;
use crate::tools::command_output_artifact::ARTIFACT_SEARCH_MAX_QUERY_BYTES;
use crate::tools::command_output_artifact::ARTIFACT_SEARCH_MAX_RESULTS;

pub(crate) const READ_TOOL_OUTPUT_TOOL_NAME: &str = "read_tool_output";

pub(crate) fn create_read_tool_output_tool() -> ToolSpec {
    let selector_schema = JsonSchema::one_of(
        vec![
            selector_variant(
                "bytes",
                BTreeMap::from([
                    (
                        "start".to_string(),
                        JsonSchema::integer(Some("Zero-based start byte.".to_string())),
                    ),
                    (
                        "end".to_string(),
                        JsonSchema::integer(Some("Exclusive end byte.".to_string())),
                    ),
                ]),
                vec!["start", "end"],
            ),
            selector_variant(
                "lines",
                BTreeMap::from([
                    (
                        "start".to_string(),
                        JsonSchema::integer(Some("One-based first line.".to_string())),
                    ),
                    (
                        "end".to_string(),
                        JsonSchema::integer(Some("Inclusive last line.".to_string())),
                    ),
                ]),
                vec!["start", "end"],
            ),
            selector_variant(
                "section",
                BTreeMap::from([(
                    "id".to_string(),
                    JsonSchema::string(Some(
                        "Stable section ID advertised by the original projection.".to_string(),
                    )),
                )]),
                vec!["id"],
            ),
            selector_variant(
                "json_pointer",
                BTreeMap::from([(
                    "pointer".to_string(),
                    JsonSchema::string(Some(
                        "RFC 6901 pointer; the empty string selects the root.".to_string(),
                    )),
                )]),
                vec!["pointer"],
            ),
            selector_variant(
                "search",
                BTreeMap::from([
                    (
                        "query".to_string(),
                        JsonSchema::string(Some(format!(
                            "Case-sensitive fixed string to find; at most {ARTIFACT_SEARCH_MAX_QUERY_BYTES} UTF-8 bytes."
                        ))),
                    ),
                    (
                        "start_byte".to_string(),
                        bounded_integer(
                            0,
                            u64::MAX,
                            "Zero-based byte at which to begin searching.".to_string(),
                        ),
                    ),
                    (
                        "max_results".to_string(),
                        bounded_integer(
                            1,
                            ARTIFACT_SEARCH_MAX_RESULTS as u64,
                            format!(
                                "Maximum matches to index; defaults to 20 and may not exceed {ARTIFACT_SEARCH_MAX_RESULTS}."
                            ),
                        ),
                    ),
                    (
                        "context_lines".to_string(),
                        bounded_integer(
                            0,
                            ARTIFACT_SEARCH_MAX_CONTEXT_LINES as u64,
                            format!(
                                "Lines of context to include in returned exact line selectors; may not exceed {ARTIFACT_SEARCH_MAX_CONTEXT_LINES}."
                            ),
                        ),
                    ),
                ]),
                vec!["query"],
            ),
        ],
        Some("Ordered search or exact-select operations over the original artifact.".to_string()),
    );
    ToolSpec::Function(ResponsesApiTool {
        name: READ_TOOL_OUTPUT_TOOL_NAME.to_string(),
        description: "Search or select from one immutable tool-output artifact without rerunning its producer. A search selector returns its merged exact contexts in hydrated_ranges in the same call; child_selectors remain as exact recovery receipts and continuation advances only when another bounded page exists. Batch adjacent or independent line, byte, section, and JSON-pointer selectors instead of rereading tiny fragments. Exact values are never clipped: oversized selectors return selector_too_large with canonical ranges and deterministic byte-subdivision plans; later independently fitting selectors may be aggregate_omitted. Byte results are base64. Recovery is deterministically reused and never spills or creates a child artifact."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([
                (
                    "artifact_id".to_string(),
                    JsonSchema::string(Some("Opaque UUID from the original tool projection.".to_string())),
                ),
                (
                    "selectors".to_string(),
                    JsonSchema::array(selector_schema, Some("Preferred ordered selector list.".to_string())),
                ),
                ("start_line".to_string(), JsonSchema::integer(Some("Legacy first 1-based line.".to_string()))),
                ("end_line".to_string(), JsonSchema::integer(Some("Legacy inclusive last line.".to_string()))),
                (
                    "ranges".to_string(),
                    JsonSchema::array(
                        JsonSchema::object(
                            BTreeMap::from([
                                ("start_line".to_string(), JsonSchema::integer(None)),
                                ("end_line".to_string(), JsonSchema::integer(None)),
                            ]),
                            Some(vec!["start_line".to_string(), "end_line".to_string()]),
                            Some(false.into()),
                        ),
                        Some("Up to 16 legacy line ranges normalized into selectors.".to_string()),
                    ),
                ),
                ("max_bytes".to_string(), JsonSchema::integer(Some("Legacy compatibility field; validated but never used to clip a selected value.".to_string()))),
            ]),
            Some(vec!["artifact_id".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn selector_variant(
    kind: &str,
    mut properties: BTreeMap<String, JsonSchema>,
    required: Vec<&str>,
) -> JsonSchema {
    properties.insert(
        "kind".to_string(),
        JsonSchema::string_enum(vec![serde_json::Value::String(kind.to_string())], None),
    );
    let mut required = required.into_iter().map(str::to_string).collect::<Vec<_>>();
    required.insert(0, "kind".to_string());
    JsonSchema::object(properties, Some(required), Some(false.into()))
}

fn bounded_integer(minimum: u64, maximum: u64, description: String) -> JsonSchema {
    JsonSchema {
        minimum: Some(Number::from(minimum)),
        maximum: Some(Number::from(maximum)),
        ..JsonSchema::integer(Some(description))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_recovery_tool_exposes_search_and_exact_select_operations() {
        let tool = serde_json::to_value(create_read_tool_output_tool())
            .expect("serialize read_tool_output spec");
        let selectors = tool
            .pointer("/parameters/properties/selectors/items/oneOf")
            .and_then(serde_json::Value::as_array)
            .expect("selector variants");
        let kinds = selectors
            .iter()
            .filter_map(|selector| selector.pointer("/properties/kind/enum/0"))
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec!["bytes", "lines", "section", "json_pointer", "search"]
        );
        assert_eq!(
            tool.pointer(
                "/parameters/properties/selectors/items/oneOf/4/properties/max_results/maximum"
            ),
            Some(&serde_json::json!(ARTIFACT_SEARCH_MAX_RESULTS)),
        );
        let description = tool["description"].as_str().expect("tool description");
        assert!(description.contains("Search or select"));
        assert!(description.contains("hydrated_ranges"));
        assert!(description.contains("same call"));
        assert!(description.contains("instead of rereading tiny fragments"));
        assert!(description.contains("deterministically reused"));
    }
}
