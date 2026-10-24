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
pub(crate) const READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES: usize = 1024 * 1024;
pub(crate) const READ_TOOL_OUTPUT_MAX_SELECTORS: usize = 64;
pub(crate) const READ_TOOL_OUTPUT_MAX_LEGACY_RANGES: usize = READ_TOOL_OUTPUT_MAX_SELECTORS;

pub(crate) fn tool_output_selector_schema() -> JsonSchema {
    selector_schema(true)
}

pub(crate) fn file_selector_schema() -> JsonSchema {
    let mut schema = selector_schema(true);
    if let Some(variants) = &mut schema.one_of
        && let Some(section) = variants.iter_mut().find(|variant| {
            variant
                .properties
                .as_ref()
                .and_then(|properties| properties.get("kind"))
                .and_then(|kind| kind.enum_values.as_ref())
                .is_some_and(|values| values == &[serde_json::json!("section")])
        })
        && let Some(id) = section
            .properties
            .as_mut()
            .and_then(|properties| properties.get_mut("id"))
    {
        id.description = Some("Use outline for a paged Rust/Python item listing (names, signatures, lines, tests, explicit module paths, and unverified linked_path_candidates), then a returned item ID for exact source. Imports are not listed; the first page gives their line runs as import_ranges. Verify module candidates through the selected filesystem only when following them. IDs are source-hash-bound; read_tool_output preserves them on the original snapshot. Ordinary reads do not parse code.".to_string());
    }
    if let Some(variants) = &mut schema.one_of {
        variants.push(selector_variant(
            "symbol",
            BTreeMap::from([("name".to_string(), JsonSchema::string(Some(
                "Unique Rust/Python item name, optionally qualified (Type::method, <Type as Trait>::method, module::Type::method, or Class.method). Exact qualified names precede suffix matches. Rust impl blocks use impl Type or impl Trait for Type, optionally module-qualified; duplicate blocks remain ambiguous. Batch known symbols and enclosing searches to avoid search-then-read calls. No type resolution or macro expansion; text hits are not semantic references.".to_string(),
            )))]),
            vec!["name"],
        ));
        variants.push(selector_variant(
            "enclosing",
            BTreeMap::from([("line".to_string(), bounded_integer(1, u64::MAX,
                "One-based line in the smallest enclosing Rust/Python item. Returns complete source lines, including Rust docs/attributes or Python decorators.".to_string(),
            ))]),
            vec!["line"],
        ));
    }
    schema
}

fn selector_schema(include_sections: bool) -> JsonSchema {
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
    if include_sections {
        variants.push(selector_variant(
            "section",
            BTreeMap::from([(
                "id".to_string(),
                JsonSchema::string(Some(
                    "Stable section ID advertised by the original projection. A Rust/Python snapshot saved with an outline lists item IDs at id outline; select an item ID where read_file takes a symbol name.".to_string(),
                )),
            )]),
            vec!["id"],
        ));
    }
    variants.push(selector_variant(
        "json_pointer",
        BTreeMap::from([(
            "pointer".to_string(),
            JsonSchema::string(Some(
                "RFC 6901 pointer; the empty string selects the root.".to_string(),
            )),
        )]),
        vec!["pointer"],
    ));
    variants.push(
            selector_variant(
                "search",
                BTreeMap::from([
                    (
                        "query".to_string(),
                        JsonSchema::string(Some(format!(
                            "Fixed string to find, case-sensitive unless case_insensitive is true; at most {ARTIFACT_SEARCH_MAX_QUERY_BYTES} UTF-8 bytes."
                        ))),
                    ),
                    (
                        "case_insensitive".to_string(),
                        JsonSchema::boolean(Some("Match ASCII letters without regard to case; defaults to false. Non-ASCII bytes remain exact.".to_string())),
                    ),
                    (
                        "enclosing".to_string(),
                        JsonSchema::boolean(Some("Also hydrate complete enclosing Rust/Python items for this bounded page; defaults to false. Continuations preserve this option. Non-item hits retain ordinary context; check enclosing_complete and selector errors.".to_string())),
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
                            u64::MAX,
                            format!(
                                "Maximum matches to index; defaults to 20 and is capped at {ARTIFACT_SEARCH_MAX_RESULTS}."
                            ),
                        ),
                    ),
                    (
                        "context_lines".to_string(),
                        bounded_integer(
                            0,
                            u64::MAX,
                            format!(
                                "Lines of context to include in returned exact line selectors; defaults to 3 and is capped at {ARTIFACT_SEARCH_MAX_CONTEXT_LINES}."
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
        "Opaque UUID from a tool projection retained by this thread; other agents cannot read it.".to_string(),
    ));
    let selectors = bounded_array(
        selector_schema.clone(),
        1,
        READ_TOOL_OUTPUT_MAX_SELECTORS as u64,
        "Exact duplicates and overlapping or adjacent same-kind ranges are normalized into stable canonical source order.".to_string(),
    );

    ToolSpec::Function(ResponsesApiTool {
        name: READ_TOOL_OUTPUT_TOOL_NAME.to_string(),
        description: "Read a saved tool-output snapshot without rerunning the tool. Artifacts are thread-private: passing an ID to another agent does not grant access. Have the owning agent recover and share the needed evidence instead. Batch known searches or selections in one call to share authenticated snapshot validation. Search results include matching text in results[].value.hydrated_ranges; a complete search page can still have more matches. Returns exact text or data_base64 for arbitrary bytes up to max_bytes plus continuation metadata; complete indicates requested-selection delivery, not full capture or current workspace freshness. Command logs retain full output on disk and support line ranges only. Source snapshots retain advanced selectors. Capture gaps and disk failures remain explicitly incomplete. Check retained_artifact_complete and continuation_stop.reason/resumable; preserve successful siblings and retry only missing selections. In scripts consume recovered data and print only needed evidence to avoid another enclosing display truncation. Never rerun the producer merely to recover output.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([
                ("artifact_id".to_string(), artifact_id),
                ("selectors".to_string(), selectors),
                ("max_bytes".to_string(), bounded_integer(1, u64::MAX,
                    format!("Maximum delivered source bytes, default {READ_TOOL_OUTPUT_MAX_BYTES}. Direct calls are capped at that default. Code-mode scripts may explicitly request up to {READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES} bytes under a separate 1 MiB serialized payload cap; print only the needed evidence. Metadata/escaping can produce a smaller page with continuation."))),
            ]),
            Some(vec!["artifact_id".to_string(), "selectors".to_string()]),
            Some(false.into()),
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
            "historical_authenticity": {"type": "string", "enum": ["authenticated", "unverified"], "description": "Whether retained bytes match a producer-time digest. Independent of complete delivery, full capture, or current workspace freshness. Missing legacy receipts are unverified."},
            "canonical_bytes": {"type": "integer", "minimum": 0},
            "retained_bytes": {"type": "integer", "minimum": 0},
            "complete": {"type": "boolean"},
            "selection_status": {"type": "string", "enum": ["complete", "partial", "failed"], "description": "Requested-page delivery, not transport success or whole-file coverage. Partial and failed batches retain all successful siblings."},
            "retained_artifact_complete": {"type": "boolean", "description": "All original bytes are retained. This does not imply the requested selection was delivered."},
            "delivered_selection_complete": {"type": "boolean", "description": "All requested selectors were delivered completely; legacy complete has the same meaning."},
            "criterion_evidence": {
                "type": "object",
                "description": "Durable source_inspection evidence reference for the original active typed attempt's exact inspect:path obligation, after all exact native snapshot bytes were acquired and the authorized current file matched its hash. Also discoverable in get_agent_task.validation_calls. Search hydration, listings and retained bytes alone are not acquisition; this is not semantic correctness or a continuing freshness guarantee.",
                "properties": {
                    "call_id": {"type": "string"}, "workspace_id": {"type": "string"},
                    "evidence_epoch": {"type": "integer", "minimum": 0},
                    "kind": {"const": "source_inspection"}
                },
                "required": ["call_id", "workspace_id", "evidence_epoch", "kind"],
                "additionalProperties": false
            },
            "inspection_proof_error": {"type": "string", "description": "The native selection remains usable, but source-inspection proof could not be published (for example, the source changed or persistence failed). Never interpret this as an acquisition receipt."},
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
                    "message": {"type": "string"},
                    "page_selectors": {"type": "array", "maxItems": 8, "items": {"$ref": "#/$defs/selector"}, "description": "First up to eight independent pages of the unconsumed selector. Fetch in parallel; selector still describes the full remainder."}
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
                    "coverage_complete": {"type": "boolean", "description": "The original source is fully retained. False means zero matches cannot establish absence; counts describe only retained bytes from start_byte."},
                    "total_matches": {"type": "integer", "minimum": 0},
                    "matches_returned": {"type": "integer", "minimum": 0},
                    "remaining_match_count": {"type": "integer", "minimum": 0},
                    "search_exhausted": {"type": "boolean", "description": "No matches remain after this page within retained source. Independent of requested-page complete and coverage_complete."},
                    "enclosing_complete": {"type": "boolean", "description": "When enclosing was requested, all enclosing sections for returned hits were resolved and hydrated. False never claims ordinary context is a complete item."},
                    "matches": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "line": {"type": "integer", "minimum": 1},
                                "end_line": {"type": "integer", "minimum": 1},
                                "start_byte": {"type": "integer", "minimum": 0},
                                "end_byte": {"type": "integer", "minimum": 0},
                                "enclosing_item": {
                                    "type": "object",
                                    "description": "With enclosing:true, the smallest Rust/Python item containing the hit; parser-derived, not type resolution. Absent for imports and hits outside any item.",
                                    "properties": {
                                        "qualified_name": {"type": "string"}, "kind": {"type": "string"},
                                        "start_line": {"type": "integer", "minimum": 1},
                                        "end_line": {"type": "integer", "minimum": 1},
                                        "syntax_error": {"const": true, "description": "The item or an owner header contains a parse error; its qualified identity or range may be wrong. Exact source delivery can still be complete."}
                                    },
                                    "required": ["qualified_name", "kind", "start_line", "end_line"],
                                    "additionalProperties": false
                                }
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
                                "shared": {"type": "boolean", "enum": [true], "description": "The exact bytes are contained in a text or data_base64 canonical_range in an earlier result of this same response. Slice that evidence by byte offset; no further read is needed. Overlapping contexts may be split into byte selectors. This never refers to another response or to a shared reference without bytes."},
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
                    .is_some_and(|values| {
                        values == &vec![serde_json::json!("search")]
                            || values == &vec![serde_json::json!("search_index")]
                    })
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
            Some(&serde_json::json!((1_u64 << 53) - 1)),
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
            &spec
                .output_schema
                .as_ref()
                .expect("recovery output schema")
                .to_value(),
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
    fn artifact_recovery_advertises_only_canonical_selectors() {
        let tool = serde_json::to_value(create_read_tool_output_tool()).expect("tool spec");
        let validator = jsonschema::validator_for(&tool["parameters"]).expect("artifact schema");
        assert!(validator.is_valid(&serde_json::json!({
            "artifact_id": "artifact", "selectors": [{"kind": "lines", "start": 1, "end": 10}]
        })));
        for legacy in [
            serde_json::json!({"artifact_id": "artifact", "start_line": 1, "end_line": 10}),
            serde_json::json!({"artifact_id": "artifact", "ranges": [{"start_line": 1, "end_line": 10}]}),
            serde_json::json!({"artifact_id": "artifact"}),
        ] {
            assert!(!validator.is_valid(&legacy));
        }
    }
}
