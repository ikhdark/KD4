use serde_json::Value as JsonValue;
use std::collections::HashSet;

use super::metadata::normalize_code_mode_identifier;

pub fn render_json_schema_to_typescript(schema: &JsonValue) -> String {
    render_json_schema_to_typescript_inner(schema, schema, &mut HashSet::new())
}

fn render_json_schema_to_typescript_inner(
    schema: &JsonValue,
    root: &JsonValue,
    active_refs: &mut HashSet<String>,
) -> String {
    match schema {
        JsonValue::Bool(true) => "unknown".to_string(),
        JsonValue::Bool(false) => "never".to_string(),
        JsonValue::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(JsonValue::as_str) {
                return render_local_schema_ref(reference, root, active_refs);
            }

            if let Some(value) = map.get("const") {
                return render_json_schema_literal(value);
            }

            if let Some(values) = map.get("enum").and_then(JsonValue::as_array) {
                let rendered = values
                    .iter()
                    .map(render_json_schema_literal)
                    .collect::<Vec<_>>();
                if !rendered.is_empty() {
                    return rendered.join(" | ");
                }
            }

            for key in ["anyOf", "oneOf"] {
                if let Some(variants) = map.get(key).and_then(JsonValue::as_array) {
                    let rendered = variants
                        .iter()
                        .map(|variant| {
                            render_json_schema_to_typescript_inner(variant, root, active_refs)
                        })
                        .collect::<Vec<_>>();
                    if !rendered.is_empty() {
                        return rendered.join(" | ");
                    }
                }
            }

            if let Some(variants) = map.get("allOf").and_then(JsonValue::as_array) {
                let rendered = variants
                    .iter()
                    .map(|variant| {
                        render_json_schema_to_typescript_inner(variant, root, active_refs)
                    })
                    .collect::<Vec<_>>();
                if !rendered.is_empty() {
                    return rendered.join(" & ");
                }
            }

            if let Some(schema_type) = map.get("type") {
                if let Some(types) = schema_type.as_array() {
                    let rendered = types
                        .iter()
                        .filter_map(JsonValue::as_str)
                        .map(|schema_type| {
                            render_json_schema_type_keyword(map, schema_type, root, active_refs)
                        })
                        .collect::<Vec<_>>();
                    if !rendered.is_empty() {
                        return rendered.join(" | ");
                    }
                }

                if let Some(schema_type) = schema_type.as_str() {
                    return render_json_schema_type_keyword(map, schema_type, root, active_refs);
                }
            }

            if map.contains_key("properties")
                || map.contains_key("additionalProperties")
                || map.contains_key("required")
            {
                return render_json_schema_object(map, root, active_refs);
            }

            if map.contains_key("items") || map.contains_key("prefixItems") {
                return render_json_schema_array(map, root, active_refs);
            }

            "unknown".to_string()
        }
        _ => "unknown".to_string(),
    }
}

fn render_local_schema_ref(
    reference: &str,
    root: &JsonValue,
    active_refs: &mut HashSet<String>,
) -> String {
    let Some(pointer) = reference.strip_prefix('#') else {
        return "unknown".to_string();
    };
    let Some(target) = (if pointer.is_empty() {
        Some(root)
    } else {
        root.pointer(pointer)
    }) else {
        return "unknown".to_string();
    };
    if !active_refs.insert(reference.to_string()) {
        return "unknown".to_string();
    }
    let rendered = render_json_schema_to_typescript_inner(target, root, active_refs);
    active_refs.remove(reference);
    rendered
}

fn render_json_schema_type_keyword(
    map: &serde_json::Map<String, JsonValue>,
    schema_type: &str,
    root: &JsonValue,
    active_refs: &mut HashSet<String>,
) -> String {
    match schema_type {
        "string" => "string".to_string(),
        "number" => render_numeric_schema(map, /*integer*/ false),
        "integer" => render_numeric_schema(map, /*integer*/ true),
        "boolean" => "boolean".to_string(),
        "null" => "null".to_string(),
        "array" => render_json_schema_array(map, root, active_refs),
        "object" => render_json_schema_object(map, root, active_refs),
        _ => "unknown".to_string(),
    }
}

fn render_numeric_schema(map: &serde_json::Map<String, JsonValue>, integer: bool) -> String {
    let mut constraints = Vec::new();
    if integer {
        constraints.push("integer".to_string());
    }
    for (keyword, label) in [
        ("minimum", "minimum"),
        ("maximum", "maximum"),
        ("exclusiveMinimum", "exclusiveMinimum"),
        ("exclusiveMaximum", "exclusiveMaximum"),
        ("multipleOf", "multipleOf"),
    ] {
        if let Some(value) = map.get(keyword) {
            constraints.push(format!("{label}: {value}"));
        }
    }
    if constraints.is_empty() {
        "number".to_string()
    } else {
        format!("number /* {} */", constraints.join("; "))
    }
}

fn render_json_schema_array(
    map: &serde_json::Map<String, JsonValue>,
    root: &JsonValue,
    active_refs: &mut HashSet<String>,
) -> String {
    if let Some(items) = map.get("items") {
        let item_type = render_json_schema_to_typescript_inner(items, root, active_refs);
        return format!("Array<{item_type}>");
    }

    if let Some(items) = map.get("prefixItems").and_then(JsonValue::as_array) {
        let item_types = items
            .iter()
            .map(|item| render_json_schema_to_typescript_inner(item, root, active_refs))
            .collect::<Vec<_>>();
        if !item_types.is_empty() {
            return format!("[{}]", item_types.join(", "));
        }
    }

    "unknown[]".to_string()
}

fn append_additional_properties_line(
    lines: &mut Vec<String>,
    map: &serde_json::Map<String, JsonValue>,
    properties: &serde_json::Map<String, JsonValue>,
    line_prefix: &str,
    root: &JsonValue,
    active_refs: &mut HashSet<String>,
) {
    if let Some(additional_properties) = map.get("additionalProperties") {
        let property_type = match additional_properties {
            JsonValue::Bool(true) => Some("unknown".to_string()),
            JsonValue::Bool(false) => None,
            value => Some(render_json_schema_to_typescript_inner(
                value,
                root,
                active_refs,
            )),
        };

        if let Some(property_type) = property_type {
            lines.push(format!("{line_prefix}[key: string]: {property_type};"));
        }
    } else if properties.is_empty() {
        lines.push(format!("{line_prefix}[key: string]: unknown;"));
    }
}

fn has_property_description(value: &JsonValue) -> bool {
    value
        .get("description")
        .and_then(JsonValue::as_str)
        .is_some_and(|description| !description.is_empty())
}

fn render_json_schema_object_property(
    name: &str,
    value: &JsonValue,
    required: &[&str],
    root: &JsonValue,
    active_refs: &mut HashSet<String>,
) -> String {
    let optional = if required.iter().any(|required_name| required_name == &name) {
        ""
    } else {
        "?"
    };
    let property_name = render_json_schema_property_name(name);
    let property_type = render_json_schema_to_typescript_inner(value, root, active_refs);
    format!("{property_name}{optional}: {property_type};")
}

fn render_json_schema_object(
    map: &serde_json::Map<String, JsonValue>,
    root: &JsonValue,
    active_refs: &mut HashSet<String>,
) -> String {
    let required = map
        .get("required")
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(JsonValue::as_str)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let properties = map
        .get("properties")
        .and_then(JsonValue::as_object)
        .cloned()
        .unwrap_or_default();

    let mut sorted_properties = properties.iter().collect::<Vec<_>>();
    sorted_properties.sort_unstable_by_key(|(name_a, _)| *name_a);
    if sorted_properties
        .iter()
        .any(|(_, value)| has_property_description(value))
    {
        let mut lines = vec!["{".to_string()];
        for (name, value) in sorted_properties {
            if let Some(description) = value.get("description").and_then(JsonValue::as_str) {
                for description_line in description
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                {
                    lines.push(format!("  // {description_line}"));
                }
            }

            lines.push(format!(
                "  {}",
                render_json_schema_object_property(name, value, &required, root, active_refs)
            ));
        }

        append_additional_properties_line(&mut lines, map, &properties, "  ", root, active_refs);
        lines.push("}".to_string());
        return lines.join("\n");
    }

    let mut lines = sorted_properties
        .into_iter()
        .map(|(name, value)| {
            render_json_schema_object_property(name, value, &required, root, active_refs)
        })
        .collect::<Vec<_>>();

    append_additional_properties_line(&mut lines, map, &properties, "", root, active_refs);

    if lines.is_empty() {
        return "{}".to_string();
    }

    format!("{{ {} }}", lines.join(" "))
}

fn render_json_schema_property_name(name: &str) -> String {
    if normalize_code_mode_identifier(name) == name {
        name.to_string()
    } else {
        JsonValue::String(name.to_string()).to_string()
    }
}

fn render_json_schema_literal(value: &JsonValue) -> String {
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_json_strings_and_literals_without_serialization_recovery() {
        assert_eq!(
            render_json_schema_property_name("line\n\"item"),
            "\"line\\n\\\"item\""
        );
        assert_eq!(
            render_json_schema_literal(&json!({"line": "one\ntwo"})),
            r#"{"line":"one\ntwo"}"#
        );
    }

    #[test]
    fn renders_local_refs_with_required_fields_and_enums() {
        let schema = json!({
            "$ref": "#/$defs/request",
            "$defs": {
                "request": {
                    "type": "object",
                    "properties": {
                        "mode": { "$ref": "#/$defs/mode" },
                        "label": { "type": "string" }
                    },
                    "required": ["mode"]
                },
                "mode": {
                    "type": "string",
                    "enum": ["fast", "safe"]
                }
            }
        });

        assert_eq!(
            render_json_schema_to_typescript(&schema),
            r#"{ label?: string; mode: "fast" | "safe"; }"#
        );
    }

    #[test]
    fn renders_integer_and_numeric_constraints() {
        assert_eq!(
            render_json_schema_to_typescript(&json!({
                "type": "integer",
                "minimum": 1,
                "maximum": 20,
                "exclusiveMinimum": 0,
                "exclusiveMaximum": 21,
                "multipleOf": 1
            })),
            "number /* integer; minimum: 1; maximum: 20; exclusiveMinimum: 0; exclusiveMaximum: 21; multipleOf: 1 */"
        );
        assert_eq!(
            render_json_schema_to_typescript(&json!({
                "type": "number",
                "minimum": 0.5
            })),
            "number /* minimum: 0.5 */"
        );
    }
}
