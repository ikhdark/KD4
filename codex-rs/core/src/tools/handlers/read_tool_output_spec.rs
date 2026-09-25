use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Number;
use std::collections::BTreeMap;

use crate::tools::command_output_artifact::ARTIFACT_SEARCH_MAX_CONTEXT_LINES;
use crate::tools::command_output_artifact::ARTIFACT_SEARCH_MAX_QUERY_BYTES;
use crate::tools::command_output_artifact::ARTIFACT_SEARCH_MAX_RESULTS;

pub(crate) const READ_TOOL_OUTPUT_TOOL_NAME: &str = "read_tool_output";
pub(crate) const READ_TOOL_OUTPUT_MAX_BYTES: usize = 16_384;
pub(crate) const READ_TOOL_OUTPUT_MAX_SELECTORS: usize = 64;
pub(crate) const READ_TOOL_OUTPUT_MAX_LEGACY_RANGES: usize = READ_TOOL_OUTPUT_MAX_SELECTORS;

pub(crate) fn tool_output_selector_schema() -> JsonSchema {
    selector_schema(true)
}

pub(crate) fn file_selector_schema() -> JsonSchema {
    selector_schema(false)
}

fn selector_schema(include_structured: bool) -> JsonSchema {
    let mut variants = vec![
        selector_variant(
            "bytes",
            BTreeMap::from([
                (
                    "start".to_string(),
                    bounded_integer(0, u64::MAX, "Zero-based start byte.".to_string()),
                ),
                (
                    "end".to_string(),
                    bounded_integer(0, u64::MAX, "Exclusive end byte.".to_string()),
                ),
            ]),
            vec!["start", "end"],
        ),
        selector_variant(
            "lines",
            BTreeMap::from([
                (
                    "start".to_string(),
                    bounded_integer(1, u64::MAX, "One-based first line.".to_string()),
                ),
                (
                    "end".to_string(),
                    bounded_integer(1, u64::MAX, "Inclusive last line.".to_string()),
                ),
            ]),
            vec!["start", "end"],
        ),
    ];
    if include_structured {
        variants.extend([
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
        ]);
    }
    variants.push(
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
            )
    );
    JsonSchema::one_of(
        variants,
        Some("Ordered search or exact-select operations over the original artifact.".to_string()),
    )
}

pub(crate) fn create_read_tool_output_tool() -> ToolSpec {
    let selector_schema = tool_output_selector_schema();
    let artifact_id = JsonSchema::string(Some(
        "Opaque UUID from the original tool projection.".to_string(),
    ));
    let selectors = bounded_array(
        selector_schema.clone(),
        1,
        READ_TOOL_OUTPUT_MAX_SELECTORS as u64,
        "Preferred selector list; exact duplicates and overlapping or adjacent same-kind ranges are normalized into stable canonical source order.".to_string(),
    );
    let legacy_start_line = bounded_integer(
        1,
        usize::MAX as u64,
        "Legacy first 1-based line.".to_string(),
    );
    let legacy_end_line = bounded_integer(
        1,
        usize::MAX as u64,
        "Legacy inclusive last line.".to_string(),
    );
    let legacy_ranges = bounded_array(
        JsonSchema::object(
            BTreeMap::from([
                (
                    "start_line".to_string(),
                    bounded_integer(1, usize::MAX as u64, "One-based first line.".to_string()),
                ),
                (
                    "end_line".to_string(),
                    bounded_integer(1, usize::MAX as u64, "Inclusive last line.".to_string()),
                ),
            ]),
            Some(vec!["start_line".to_string(), "end_line".to_string()]),
            Some(false.into()),
        ),
        1,
        READ_TOOL_OUTPUT_MAX_LEGACY_RANGES as u64,
        format!(
            "Up to {READ_TOOL_OUTPUT_MAX_LEGACY_RANGES} legacy line ranges normalized into selectors."
        ),
    );
    let input_variant = |variant_properties: Vec<(String, JsonSchema)>, required: Vec<&str>| {
        let mut properties = BTreeMap::from([("artifact_id".to_string(), artifact_id.clone())]);
        properties.extend(variant_properties);
        JsonSchema::object(
            properties,
            Some(required.into_iter().map(str::to_string).collect()),
            Some(false.into()),
        )
    };

    ToolSpec::Function(ResponsesApiTool {
        name: READ_TOOL_OUTPUT_TOOL_NAME.to_string(),
        description: "Read a saved tool-output snapshot without rerunning the tool. Batch independent searches or selections in one call. Search results include matching text in results[].value.hydrated_ranges. Selected values are returned intact; oversized selections return smaller child_selectors or a continuation selector to retry. complete indicates whether all requested selections were returned. If continuation_stop is present, check its reason and resumable fields before retrying.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::one_of(
            vec![
                input_variant(
                    vec![("selectors".to_string(), selectors)],
                    vec!["artifact_id", "selectors"],
                ),
                input_variant(
                    vec![("ranges".to_string(), legacy_ranges)],
                    vec!["artifact_id", "ranges"],
                ),
                input_variant(
                    vec![
                        ("start_line".to_string(), legacy_start_line),
                        ("end_line".to_string(), legacy_end_line),
                    ],
                    vec!["artifact_id"],
                ),
            ],
            Some(
                "Use selectors, legacy ranges, or the legacy single-line range form; do not mix forms."
                    .to_string(),
            ),
        ),
        output_schema: Some(read_tool_output_output_schema(selector_schema).into()),
    })
}

pub(crate) fn read_tool_output_output_schema(mut selector_schema: JsonSchema) -> serde_json::Value {
    // Invalid selectors are echoed in error results, so output coordinates must
    // describe the parsed unsigned values without imposing input validity bounds.
    for variant in selector_schema.one_of.iter_mut().flatten() {
        for property in variant
            .properties
            .iter_mut()
            .flat_map(|properties| properties.values_mut())
        {
            if property.minimum.is_some() {
                property.minimum = Some(Number::from(0));
                property.maximum = None;
            }
        }
    }
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": {
            "artifact_id": {"type": "string"},
            "canonical_sha256": {"type": "string"},
            "canonical_bytes": {"type": "integer", "minimum": 0},
            "retained_bytes": {"type": "integer", "minimum": 0},
            "complete": {"type": "boolean"},
            "retained_artifact_complete": {"type": "boolean", "description": "All original bytes are retained. This does not imply the requested selection was delivered."},
            "delivered_selection_complete": {"type": "boolean", "description": "All requested selectors were delivered completely; legacy complete has the same meaning."},
            "unavailable_ranges": {"type": "array", "items": {"$ref": "#/$defs/range"}},
            "results": {"type": "array", "items": {"$ref": "#/$defs/result"}},
            "continuation_stop": {
                "type": "object",
                "properties": {
                    "version": {"type": "integer", "enum": [1]},
                    "reason": {"type": "string", "enum": [
                        "budget", "cancelled", "identity_drift", "incomplete_owner_result",
                        "invalid_selector", "selector_not_found", "page_read_error", "repeated_selector"
                    ]},
                    "selector": {"anyOf": [{"$ref": "#/$defs/selector"}, {"type": "null"}]},
                    "resumable": {"type": "boolean"},
                    "message": {"type": "string"}
                },
                "required": ["version", "reason", "selector", "resumable"],
                "additionalProperties": false
            }
        },
        "required": ["artifact_id", "canonical_sha256", "canonical_bytes", "retained_bytes", "complete", "retained_artifact_complete", "delivered_selection_complete", "results"],
        "additionalProperties": false,
        "$defs": {
            "selector": selector_schema,
            "range": {
                "type": "object",
                "properties": {"start": {"type": "integer", "minimum": 0}, "end": {"type": "integer", "minimum": 0}},
                "required": ["start", "end"],
                "additionalProperties": false
            },
            "result": {
                "type": "object",
                "properties": {
                    "selector": {"$ref": "#/$defs/selector"},
                    "status": {"type": "string", "enum": ["ok", "selector_too_large", "aggregate_omitted", "not_found", "invalid"]},
                    "complete": {"type": "boolean"},
                    "exact_bytes": {"type": "integer", "minimum": 0},
                    "canonical_range": {"$ref": "#/$defs/range"},
                    "text": {"type": "string"},
                    "value": {"description": "Exact selected JSON value. Search selectors return the search_result shape."},
                    "data_base64": {"type": "string"},
                    "subdivision_plan": {
                        "type": "object",
                        "properties": {
                            "range": {"$ref": "#/$defs/range"},
                            "chunk_bytes": {"type": "integer", "minimum": 0},
                            "chunk_count": {"type": "integer", "minimum": 0},
                            "selector_kind": {"type": "string"}
                        },
                        "required": ["range", "chunk_bytes", "chunk_count", "selector_kind"],
                        "additionalProperties": false
                    },
                    "child_selectors": {"type": "array", "items": {"$ref": "#/$defs/selector"}},
                    "continuation": {"$ref": "#/$defs/selector"},
                    "message": {"type": "string"}
                },
                "required": ["selector", "status", "complete"],
                "additionalProperties": false
            },
            "search_result": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "start_byte": {"type": "integer", "minimum": 0},
                    "coverage_complete": {"type": "boolean"},
                    "total_matches": {"type": "integer", "minimum": 0},
                    "matches_returned": {"type": "integer", "minimum": 0},
                    "remaining_match_count": {"type": "integer", "minimum": 0},
                    "matches": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "line": {"type": "integer", "minimum": 1},
                                "end_line": {"type": "integer", "minimum": 1},
                                "start_byte": {"type": "integer", "minimum": 0},
                                "end_byte": {"type": "integer", "minimum": 0}
                            },
                            "required": ["line", "end_line", "start_byte", "end_byte"],
                            "additionalProperties": false
                        }
                    },
                    "hydrated_ranges": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "selector": {"$ref": "#/$defs/selector"},
                                "canonical_range": {"$ref": "#/$defs/range"},
                                "exact_bytes": {"type": "integer", "minimum": 0},
                                "text": {"type": "string"},
                                "data_base64": {"type": "string"}
                            },
                            "required": ["selector", "canonical_range", "exact_bytes"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["query", "start_byte", "coverage_complete", "total_matches", "matches_returned", "remaining_match_count", "matches", "hydrated_ranges"],
                "additionalProperties": false
            }
        }
    });
    let mut exact_result = schema["$defs"]["result"].clone();
    let mut search_result = exact_result.clone();
    let (search_selectors, exact_selectors): (Vec<_>, Vec<_>) = selector_schema
        .one_of
        .unwrap_or_default()
        .into_iter()
        .partition(|variant| {
            variant.properties.as_ref().is_some_and(|properties| {
                properties
                    .get("kind")
                    .and_then(|kind| kind.enum_values.as_ref())
                    == Some(&vec![serde_json::json!("search")])
            })
        });
    exact_result["properties"]["selector"] = serde_json::json!({"oneOf": exact_selectors});
    search_result["properties"]["selector"] = serde_json::json!({"oneOf": search_selectors});
    search_result["properties"]["value"] = serde_json::json!({"$ref": "#/$defs/search_result"});
    schema["$defs"]["result"] = serde_json::json!({"oneOf": [exact_result, search_result]});
    schema
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
    required.push("kind".to_string());
    JsonSchema::object(properties, Some(required), Some(false.into()))
}

fn bounded_integer(minimum: u64, maximum: u64, description: String) -> JsonSchema {
    // Code-mode calls pass through JavaScript numbers, whose exact integer range ends at 2^53 - 1.
    JsonSchema {
        minimum: Some(Number::from(minimum)),
        maximum: Some(Number::from(maximum.min((1_u64 << 53) - 1))),
        ..JsonSchema::integer(Some(description))
    }
}

fn bounded_array(items: JsonSchema, minimum: u64, maximum: u64, description: String) -> JsonSchema {
    JsonSchema {
        min_items: Some(minimum),
        max_items: Some(maximum),
        ..JsonSchema::array(items, Some(description))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_range_selectors_enforce_coordinate_bounds() {
        let tool = serde_json::to_value(create_read_tool_output_tool()).expect("tool spec");
        let validator = jsonschema::validator_for(&tool["parameters"]).expect("artifact schema");
        for (kind, minimum) in [("bytes", 0), ("lines", 1)] {
            for field in ["start", "end"] {
                for value in [minimum, 9_007_199_254_740_991_i64] {
                    let mut selector =
                        serde_json::json!({"kind": kind, "start": minimum, "end": minimum});
                    selector[field] = serde_json::json!(value);
                    assert!(validator.is_valid(
                        &serde_json::json!({"artifact_id": "artifact", "selectors": [selector]})
                    ));
                }
                for value in [minimum - 1, 9_007_199_254_740_992_i64] {
                    let mut selector =
                        serde_json::json!({"kind": kind, "start": minimum, "end": minimum});
                    selector[field] = serde_json::json!(value);
                    assert!(!validator.is_valid(
                        &serde_json::json!({"artifact_id": "artifact", "selectors": [selector]})
                    ));
                }
            }
        }
    }

    #[test]
    fn artifact_recovery_tool_exposes_search_and_exact_select_operations() {
        let tool = serde_json::to_value(create_read_tool_output_tool())
            .expect("serialize read_tool_output spec");
        let selectors = tool
            .pointer("/parameters/oneOf/0/properties/selectors/items/oneOf")
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
                "/parameters/oneOf/0/properties/selectors/items/oneOf/4/properties/max_results/maximum"
            ),
            Some(&serde_json::json!(ARTIFACT_SEARCH_MAX_RESULTS)),
        );
        let validator = jsonschema::validator_for(&tool["parameters"]).expect("artifact schema");
        for selector in [
            serde_json::json!({"kind": "search", "query": "error"}),
            serde_json::json!({"kind": "section", "id": "diagnostics"}),
            serde_json::json!({"kind": "json_pointer", "pointer": "/result"}),
        ] {
            let mut args = serde_json::json!({"artifact_id": "artifact", "selectors": [selector]});
            assert!(validator.is_valid(&args));
            args["selectors"][0]
                .as_object_mut()
                .expect("selector")
                .remove("kind");
            assert!(!validator.is_valid(&args), "selector kind is required");
        }
        let ToolSpec::Function(spec) = create_read_tool_output_tool() else {
            panic!("recovery uses a function spec");
        };
        let declaration = codex_code_mode::render_json_schema_to_typescript(
            &spec.output_schema.as_ref().expect("recovery output schema").to_value(),
        );
        for field in [
            "complete: boolean",
            "hydrated_ranges:",
            "child_selectors?",
            "continuation_stop?",
        ] {
            assert!(
                declaration.contains(field),
                "missing {field} in {declaration}"
            );
        }
        assert!(
            !declaration.contains("schema projection incomplete"),
            "{declaration}"
        );
    }

    #[test]
    fn artifact_recovery_schema_keeps_canonical_and_legacy_forms_exclusive() {
        let tool = serde_json::to_value(create_read_tool_output_tool())
            .expect("serialize read_tool_output spec");
        let branches = tool["parameters"]["oneOf"]
            .as_array()
            .expect("input form branches");
        assert_eq!(branches.len(), 3);
        assert!(
            branches
                .iter()
                .all(|branch| branch["additionalProperties"] == false)
        );
        assert!(
            branches[0]["required"]
                .as_array()
                .is_some_and(|required| required.contains(&serde_json::json!("selectors")))
        );
        assert!(
            branches[1]["required"]
                .as_array()
                .is_some_and(|required| required.contains(&serde_json::json!("ranges")))
        );
        assert!(branches[2]["properties"].get("selectors").is_none());
        assert!(branches[2]["properties"].get("ranges").is_none());
        assert_eq!(branches[2]["properties"]["start_line"]["minimum"], 1);
        assert!(
            branches
                .iter()
                .all(|branch| branch["properties"].get("max_bytes").is_none())
        );
        let validator = jsonschema::validator_for(&tool["parameters"]).expect("artifact schema");
        let args = serde_json::json!({"artifact_id": "artifact", "start_line": 1, "end_line": 10});
        assert!(validator.is_valid(&args));
        let mut obsolete_budget = args.clone();
        obsolete_budget["max_bytes"] = serde_json::json!(100);
        assert!(!validator.is_valid(&obsolete_budget));
        let mut unsafe_number = args;
        unsafe_number["start_line"] = serde_json::json!(9_007_199_254_740_992_u64);
        assert!(!validator.is_valid(&unsafe_number));
    }
}
