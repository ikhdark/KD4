use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

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
        ],
        Some("Ordered exact selectors over the original artifact.".to_string()),
    );
    ToolSpec::Function(ResponsesApiTool {
        name: READ_TOOL_OUTPUT_TOOL_NAME.to_string(),
        description: "Recover exact bounded values from one immutable tool-output artifact. Values are never clipped: oversized selectors return selector_too_large with exact canonical ranges and deterministic byte-subdivision plans; later independently fitting selectors may be aggregate_omitted. Byte results are base64. Recovery never spills or creates a child artifact."
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
