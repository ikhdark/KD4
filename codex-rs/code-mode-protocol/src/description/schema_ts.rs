use serde_json::Value as JsonValue;
use std::collections::HashSet;

use super::metadata::normalize_code_mode_identifier;

pub fn render_json_schema_to_typescript(schema: &JsonValue) -> String {
    render_json_schema_to_typescript_inner(schema, schema, &mut RenderBudget::default())
        .unwrap_or_else(|()| "unknown /* schema projection incomplete: rendering limit reached; consult the tool's JSON Schema */".to_string())
}

// Bound traversal before expanding references, and account for intermediate strings
// as well as the final output. A DAG can expand exponentially without any cycles.
struct RenderBudget {
    active_refs: HashSet<String>,
    nodes: usize,
    depth: usize,
    bytes: usize,
}

impl Default for RenderBudget {
    fn default() -> Self {
        Self {
            active_refs: HashSet::new(),
            nodes: 1024,
            depth: 0,
            bytes: 128 * 1024,
        }
    }
}

impl RenderBudget {
    fn spend(&mut self, bytes: usize) -> Result<(), ()> {
        self.bytes = self.bytes.checked_sub(bytes).ok_or(())?;
        Ok(())
    }
}

type Rendered = Result<String, ()>;

const NESTED_RESOURCE_UNKNOWN: &str = "unknown /* schema projection incomplete: nested $id resource not projected; consult JSON Schema */";

fn render_json_schema_to_typescript_inner(
    schema: &JsonValue,
    root: &JsonValue,
    budget: &mut RenderBudget,
) -> Rendered {
    budget.nodes = budget.nodes.checked_sub(1).ok_or(())?;
    if budget.depth >= 64 {
        return Err(());
    }
    budget.depth += 1;
    let rendered = render_schema(schema, root, budget);
    budget.depth -= 1;
    let rendered = rendered?;
    budget.spend(rendered.len())?;
    Ok(rendered)
}

fn render_schema(schema: &JsonValue, root: &JsonValue, budget: &mut RenderBudget) -> Rendered {
    // A nested resource has its own reference identity. Until resource resolution
    // is supported, never resolve its fragments against the enclosing document.
    if !std::ptr::eq(schema, root) && schema.get("$id").and_then(JsonValue::as_str).is_some() {
        return Ok(NESTED_RESOURCE_UNKNOWN.to_string());
    }
    match schema {
        JsonValue::Bool(true) => Ok("unknown".to_string()),
        JsonValue::Bool(false) => Ok("never".to_string()),
        JsonValue::Object(map) => {
            let mut constraints = Vec::new();
            if let Some(reference) = map.get("$ref").and_then(JsonValue::as_str) {
                let rendered = render_local_schema_ref(reference, root, budget)?;
                let dialect = root
                    .get("$schema")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                if dialect.contains("2019-09") || dialect.contains("2020-12") {
                    constraints.push(rendered);
                } else {
                    // Preserve the existing Reference Object behavior for older or
                    // unspecified dialects; do not silently claim to cover siblings.
                    let has_siblings = map.keys().any(|key| {
                        matches!(
                            key.as_str(),
                            "type"
                                | "properties"
                                | "required"
                                | "additionalProperties"
                                | "items"
                                | "prefixItems"
                                | "const"
                                | "enum"
                                | "allOf"
                                | "anyOf"
                                | "oneOf"
                        )
                    });
                    return Ok(if dialect.is_empty() && has_siblings {
                        format!(
                            "{rendered} /* $ref siblings not projected: schema dialect unspecified */"
                        )
                    } else {
                        rendered
                    });
                }
            }

            if let Some(value) = map.get("const") {
                constraints.push(render_bounded_literal(value, budget)?);
            }

            if let Some(values) = map.get("enum").and_then(JsonValue::as_array) {
                let rendered = values
                    .iter()
                    .map(|value| render_bounded_literal(value, budget))
                    .collect::<Result<Vec<_>, _>>()?;
                constraints.push(if rendered.is_empty() {
                    "never".to_string()
                } else {
                    rendered.join(" | ")
                });
            }

            for key in ["anyOf", "oneOf"] {
                if let Some(variants) = map.get(key).and_then(JsonValue::as_array) {
                    let rendered = variants
                        .iter()
                        .map(|variant| {
                            render_json_schema_to_typescript_inner(variant, root, budget)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    constraints.push(if rendered.is_empty() {
                        "never".to_string()
                    } else {
                        rendered.join(" | ")
                    });
                }
            }

            if let Some(variants) = map.get("allOf").and_then(JsonValue::as_array) {
                for variant in variants {
                    constraints.push(render_json_schema_to_typescript_inner(
                        variant, root, budget,
                    )?);
                }
            }

            let mut base = None;
            if let Some(schema_type) = map.get("type") {
                if let Some(types) = schema_type.as_array() {
                    if types.len() > budget.nodes {
                        return Err(());
                    }
                    let rendered = types
                        .iter()
                        .filter_map(JsonValue::as_str)
                        .map(|schema_type| {
                            render_json_schema_type_keyword(map, schema_type, root, budget)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    if !rendered.is_empty() {
                        base = Some(rendered.join(" | "));
                    }
                }

                if let Some(schema_type) = schema_type.as_str() {
                    // Avoid redundant `string & "literal"` declarations while
                    // retaining structural and numeric constraints beside literals.
                    let implied = |value: &JsonValue| {
                        matches!(schema_type,
                        "string" if value.is_string())
                            || matches!(schema_type,
                        "boolean" if value.is_boolean())
                            || matches!(schema_type,
                        "null" if value.is_null())
                    };
                    let literal_implies_type = map.get("const").is_some_and(implied)
                        || map
                            .get("enum")
                            .and_then(JsonValue::as_array)
                            .is_some_and(|values| !values.is_empty() && values.iter().all(implied));
                    if !literal_implies_type {
                        base = Some(render_json_schema_type_keyword(
                            map,
                            schema_type,
                            root,
                            budget,
                        )?);
                    }
                }
            } else {
                let has_object = [
                    "properties",
                    "additionalProperties",
                    "required",
                    "patternProperties",
                ]
                .iter()
                .any(|key| map.contains_key(*key));
                let has_array = ["items", "prefixItems", "minItems", "maxItems"]
                    .iter()
                    .any(|key| map.contains_key(*key));
                if has_object || has_array {
                    // Applicator keywords constrain only their own instance type.
                    // Retain the other JSON types, including inside compositions.
                    let object = render_json_schema_object(map, root, budget)?;
                    let array = render_json_schema_array(map, root, budget)?;
                    base = Some(format!(
                        "string | number | boolean | null | {array} | {object}"
                    ));
                }
            }
            if let Some(base) = base {
                constraints.insert(0, base);
            }
            let rendered = match constraints.len() {
                0 => "unknown".to_string(),
                1 if !map.contains_key("allOf") => constraints.remove(0),
                _ => constraints
                    .into_iter()
                    .map(|part| format!("({part})"))
                    .collect::<Vec<_>>()
                    .join(" & "),
            };
            annotate_schema_constraints(rendered, map, budget)
        }
        _ => Ok("unknown".to_string()),
    }
}

fn render_local_schema_ref(
    reference: &str,
    root: &JsonValue,
    budget: &mut RenderBudget,
) -> Rendered {
    budget.spend(reference.len())?;
    let Some(pointer) = reference.strip_prefix('#') else {
        return Ok("unknown /* external $ref not projected */".to_string());
    };
    let Some(pointer) = decode_schema_fragment(pointer) else {
        return Ok("unknown /* invalid $ref URI fragment */".to_string());
    };
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return Ok("unknown /* unresolved $ref */".to_string());
    }
    let mut target = root;
    // Walk one escaped token at a time so pointers cannot skip over a resource
    // boundary into a descendant that no longer contains the ancestor's $id.
    for token in pointer.split('/').skip(1) {
        budget.nodes = budget.nodes.checked_sub(1).ok_or(())?;
        if !std::ptr::eq(target, root) && target.get("$id").and_then(JsonValue::as_str).is_some() {
            return Ok(NESTED_RESOURCE_UNKNOWN.to_string());
        }
        let Some(next) = target.pointer(&format!("/{token}")) else {
            return Ok("unknown /* unresolved $ref */".to_string());
        };
        target = next;
    }
    if !budget.active_refs.insert(pointer.clone()) {
        return Ok("unknown /* recursive $ref */".to_string());
    }
    let rendered = render_json_schema_to_typescript_inner(target, root, budget);
    budget.active_refs.remove(&pointer);
    rendered
}

fn decode_schema_fragment(fragment: &str) -> Option<String> {
    let mut decoded = Vec::with_capacity(fragment.len());
    let mut bytes = fragment.bytes();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            (high * 16 + low) as u8
        } else {
            byte
        });
    }
    String::from_utf8(decoded).ok()
}

fn annotate_schema_constraints(
    rendered: String,
    map: &serde_json::Map<String, JsonValue>,
    budget: &mut RenderBudget,
) -> Rendered {
    let mut annotations = Vec::new();
    for keyword in ["pattern", "minLength", "maxLength", "format"] {
        if let Some(value) = map.get(keyword) {
            let value = sanitize_comment(&render_bounded_literal(value, budget)?);
            annotations.push(format!("{keyword}: {value}"));
        }
    }
    if map.contains_key("oneOf") {
        annotations.push("oneOf: exactly one branch must match; consult JSON Schema".to_string());
    }
    for keyword in ["not", "if", "then", "else", "$dynamicRef", "$recursiveRef"] {
        if map.contains_key(keyword) {
            annotations.push(format!(
                "unprojected keyword: {keyword}; consult JSON Schema"
            ));
        }
    }
    Ok(if annotations.is_empty() {
        rendered
    } else {
        format!("{rendered} /* {} */", annotations.join("; "))
    })
}

fn render_json_schema_type_keyword(
    map: &serde_json::Map<String, JsonValue>,
    schema_type: &str,
    root: &JsonValue,
    budget: &mut RenderBudget,
) -> Rendered {
    Ok(match schema_type {
        "string" => "string".to_string(),
        "number" => render_numeric_schema(map, /*integer*/ false),
        "integer" => render_numeric_schema(map, /*integer*/ true),
        "boolean" => "boolean".to_string(),
        "null" => "null".to_string(),
        "array" => return render_json_schema_array(map, root, budget),
        "object" => return render_json_schema_object(map, root, budget),
        _ => "unknown".to_string(),
    })
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
        if let Some(value) = map.get(keyword).filter(|value| value.is_number()) {
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
    budget: &mut RenderBudget,
) -> Rendered {
    let minimum = map.get("minItems").and_then(JsonValue::as_u64).unwrap_or(0);
    let maximum = map.get("maxItems").and_then(JsonValue::as_u64);
    if maximum.is_some_and(|maximum| maximum < minimum) {
        return Ok("never".to_string());
    }
    // `items: [...]` is the tuple spelling used by older schema drafts.
    let prefix = map
        .get("prefixItems")
        .and_then(JsonValue::as_array)
        .or_else(|| map.get("items").and_then(JsonValue::as_array));
    let trailing = if map.contains_key("prefixItems") {
        map.get("items")
    } else if prefix.is_some() {
        map.get("additionalItems")
    } else {
        map.get("items")
    }
    .unwrap_or(&JsonValue::Bool(true));
    let rendered = if let Some(prefix) = prefix {
        if trailing == &JsonValue::Bool(false) && minimum > prefix.len() as u64 {
            return Ok("never".to_string());
        }
        let mut parts = Vec::new();
        for (index, item) in prefix.iter().enumerate() {
            if maximum.is_some_and(|maximum| index as u64 >= maximum) {
                break;
            }
            let part = render_json_schema_to_typescript_inner(item, root, budget)?;
            parts.push(if (index as u64) < minimum {
                part
            } else {
                format!("({part})?")
            });
        }
        if trailing != &JsonValue::Bool(false)
            && maximum.is_none_or(|maximum| maximum > prefix.len() as u64)
        {
            let tail = render_json_schema_to_typescript_inner(trailing, root, budget)?;
            parts.push(format!("...Array<{tail}>"));
        }
        format!("[{}]", parts.join(", "))
    } else if trailing == &JsonValue::Bool(false) && minimum > 0 {
        "never".to_string()
    } else if map.contains_key("items") {
        format!(
            "Array<{}>",
            render_json_schema_to_typescript_inner(trailing, root, budget)?
        )
    } else {
        "unknown[]".to_string()
    };
    let mut lengths = Vec::new();
    for keyword in ["minItems", "maxItems"] {
        if let Some(value) = map.get(keyword).and_then(JsonValue::as_u64) {
            lengths.push(format!("{keyword}: {value}"));
        }
    }
    Ok(if lengths.is_empty() {
        rendered
    } else {
        format!("{rendered} /* {} */", lengths.join("; "))
    })
}

fn has_property_description(value: &JsonValue) -> bool {
    value
        .get("description")
        .and_then(JsonValue::as_str)
        .is_some_and(|description| !description.is_empty())
}

fn render_json_schema_object(
    map: &serde_json::Map<String, JsonValue>,
    root: &JsonValue,
    budget: &mut RenderBudget,
) -> Rendered {
    let empty_properties = serde_json::Map::new();
    let properties = map
        .get("properties")
        .and_then(JsonValue::as_object)
        .unwrap_or(&empty_properties);
    let required_items = map.get("required").and_then(JsonValue::as_array);
    if properties
        .len()
        .saturating_add(required_items.map_or(0, Vec::len))
        > budget.nodes
    {
        return Err(());
    }
    let required = map
        .get("required")
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(JsonValue::as_str)
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    let additional = map
        .get("additionalProperties")
        .unwrap_or(&JsonValue::Bool(true));
    let has_patterns = map
        .get("patternProperties")
        .and_then(JsonValue::as_object)
        .is_some_and(|patterns| !patterns.is_empty());
    let mut names = properties
        .keys()
        .map(String::as_str)
        .chain(required.iter().copied())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    let multiline = properties.values().any(has_property_description);
    let mut lines = Vec::new();
    for name in &names {
        // patternProperties may constrain a required key even when it is absent
        // from properties. Do not declare that case impossible without matching it.
        let value = properties.get(*name).unwrap_or(if has_patterns {
            &JsonValue::Bool(true)
        } else {
            additional
        });
        if required.contains(name) && value == &JsonValue::Bool(false) {
            return Ok("never".to_string());
        }
        if multiline && let Some(description) = value.get("description").and_then(JsonValue::as_str)
        {
            budget.spend(description.len())?;
            for description_line in description
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
            {
                lines.push(format!("// {description_line}"));
            }
        }
        budget.spend(name.len().saturating_mul(6))?;
        let property_name = render_json_schema_property_name(name);
        let property_type = render_json_schema_to_typescript_inner(value, root, budget)?;
        let optional = if required.contains(name) { "" } else { "?" };
        lines.push(format!("{property_name}{optional}: {property_type};"));
    }
    if has_patterns {
        lines.push(
            "[key: string]: unknown; /* patternProperties not projected; consult JSON Schema */"
                .to_string(),
        );
    } else if additional != &JsonValue::Bool(false)
        && (map.contains_key("additionalProperties") || properties.is_empty())
    {
        let additional_type = render_json_schema_to_typescript_inner(additional, root, budget)?;
        if names.is_empty() || additional_type == "unknown" {
            lines.push(format!("[key: string]: {additional_type};"));
        } else {
            // TS index signatures also govern named fields. An unknown signature
            // is deliberately wider; the annotation preserves the extra-key rule.
            let annotation = sanitize_comment(&additional_type);
            lines.push(format!(
                "[key: string]: unknown; /* additional keys: {annotation} */"
            ));
        }
    }
    if lines.is_empty() {
        return Ok("Record<string, never>".to_string());
    }
    Ok(if multiline {
        format!("{{\n  {}\n}}", lines.join("\n  "))
    } else {
        format!("{{ {} }}", lines.join(" "))
    })
}

fn render_json_schema_property_name(name: &str) -> String {
    if normalize_code_mode_identifier(name) == name {
        name.to_string()
    } else {
        JsonValue::String(name.to_string()).to_string()
    }
}

fn render_bounded_literal(value: &JsonValue, budget: &mut RenderBudget) -> Rendered {
    bound_literal_traversal(value, budget)?;
    // Stop serialization before a large literal is copied, without reserving the
    // whole output allowance for each ordinary enum member.
    let mut writer = BoundedLiteralWriter {
        bytes: Vec::new(),
        limit: budget.bytes,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| ())?;
    budget.spend(writer.bytes.len().max(1))?;
    String::from_utf8(writer.bytes).map_err(|_| ())
}

fn bound_literal_traversal(value: &JsonValue, budget: &mut RenderBudget) -> Result<(), ()> {
    budget.nodes = budget.nodes.checked_sub(1).ok_or(())?;
    if budget.depth >= 64 {
        return Err(());
    }
    budget.depth += 1;
    let result = match value {
        JsonValue::Array(values) => values
            .iter()
            .try_for_each(|value| bound_literal_traversal(value, budget)),
        JsonValue::Object(values) => values
            .values()
            .try_for_each(|value| bound_literal_traversal(value, budget)),
        _ => Ok(()),
    };
    budget.depth -= 1;
    result
}

struct BoundedLiteralWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl std::io::Write for BoundedLiteralWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("schema rendering limit reached"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Keep schema-controlled text within one generated block comment.
fn sanitize_comment(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut previous = None;
    for ch in value.chars() {
        if previous == Some('*') && ch == '/' {
            output.push(' ');
        }
        output.push(if matches!(ch, '\n' | '\r') { ' ' } else { ch });
        previous = Some(ch);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn composition_preserves_base_constraints_for_every_union_branch() {
        for keyword in ["anyOf", "oneOf"] {
            assert_eq!(
                render_json_schema_to_typescript(&json!({
                    "type": "object",
                    "properties": {"id": {"type": "string"}}, "required": ["id"],
                    (keyword): [{"required": ["left"]}, {"required": ["right"]}]
                })),
                format!(
                    "({{ id: string; }}) & (string | number | boolean | null | unknown[] | {{ left: unknown; [key: string]: unknown; }} | string | number | boolean | null | unknown[] | {{ right: unknown; [key: string]: unknown; }}){}",
                    if keyword == "oneOf" {
                        " /* oneOf: exactly one branch must match; consult JSON Schema */"
                    } else {
                        ""
                    }
                ),
                "{keyword} must retain id on both alternatives"
            );
        }
        assert_eq!(
            render_json_schema_to_typescript(&json!({
                "type": "integer", "minimum": 1, "const": 2, "enum": [2, 3],
                "allOf": [{"type": "number", "maximum": 4}]
            })),
            "(number /* integer; minimum: 1 */) & (2) & (2 | 3) & (number /* maximum: 4 */)"
        );
    }

    #[test]
    fn reference_siblings_follow_the_declared_dialect() {
        for (dialect, expected) in [
            (
                Some("https://json-schema.org/draft/2019-09/schema"),
                "(number /* minimum: 2 */) & (number)",
            ),
            (
                Some("https://json-schema.org/draft/2020-12/schema"),
                "(number /* minimum: 2 */) & (number)",
            ),
            (Some("http://json-schema.org/draft-07/schema#"), "number"),
            (
                None,
                "number /* $ref siblings not projected: schema dialect unspecified */",
            ),
        ] {
            let mut schema = json!({
                "$defs": {"value": {"type": "number"}},
                "$ref": "#/$defs/value", "type": "number", "minimum": 2
            });
            if let Some(dialect) = dialect {
                schema["$schema"] = json!(dialect);
            }
            assert_eq!(
                render_json_schema_to_typescript(&schema),
                expected,
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn references_distinguish_shared_targets_cycles_and_missing_targets() {
        assert_eq!(
            render_json_schema_to_typescript(&json!({
                "$defs": {"a/b~c": {"type": "string"}},
                "type": "object", "required": ["first", "second"],
                "properties": {
                    "first": {"$ref": "#/$defs/a~1b~0c"},
                    "second": {"$ref": "#/$defs/a~1b~0c"}
                }
            })),
            "{ first: string; second: string; }"
        );
        for (schema, expected) in [
            (
                json!({"$defs": {"a/b~c": {"type": "boolean"}}, "$ref": "#/%24defs/a%7E1b%7E0c"}),
                "boolean",
            ),
            (
                json!({"$defs": {"é": {"type": "string"}}, "$ref": "#/$defs/%C3%A9"}),
                "string",
            ),
            (
                json!({"$defs": {"a b": {"$ref": "#/$defs/a%20b"}}, "$ref": "#/$defs/a b"}),
                "unknown /* recursive $ref */",
            ),
            (
                json!({"$ref": "#/%FF"}),
                "unknown /* invalid $ref URI fragment */",
            ),
            (
                json!({"$ref": "#/%2"}),
                "unknown /* invalid $ref URI fragment */",
            ),
            (
                json!({"$ref": "#/%GG"}),
                "unknown /* invalid $ref URI fragment */",
            ),
            (json!({"$ref": "#"}), "unknown /* recursive $ref */"),
            (
                json!({"$defs": {"a": {"$ref": "#/$defs/b"}, "b": {"$ref": "#/$defs/a"}}, "$ref": "#/$defs/a"}),
                "unknown /* recursive $ref */",
            ),
            (
                json!({"$ref": "#/$defs/missing"}),
                "unknown /* unresolved $ref */",
            ),
            (
                json!({"$ref": "https://example.invalid/schema"}),
                "unknown /* external $ref not projected */",
            ),
        ] {
            assert_eq!(render_json_schema_to_typescript(&schema), expected);
        }
    }

    #[test]
    fn tuple_rules_distinguish_optional_prefix_closed_tail_and_legacy_items() {
        for (schema, expected) in [
            (
                json!({"type": "array", "prefixItems": [{"type": "string"}], "items": false}),
                "[(string)?]",
            ),
            (
                json!({"type": "array", "prefixItems": [{"type": "string"}], "items": {"type": "boolean"}, "minItems": 1}),
                "[string, ...Array<boolean>] /* minItems: 1 */",
            ),
            (
                json!({"type": "array", "prefixItems": [{"type": "string"}], "maxItems": 0}),
                "[] /* maxItems: 0 */",
            ),
            (
                json!({"type": "array", "minItems": 2, "maxItems": 1}),
                "never",
            ),
            (
                json!({"type": "array", "items": false, "minItems": 1}),
                "never /* minItems: 1 */",
            ),
            (
                json!({"type": "array", "items": [{"type": "string"}], "additionalItems": {"type": "number"}, "minItems": 1}),
                "[string, ...Array<number>] /* minItems: 1 */",
            ),
            // additionalItems belongs to the legacy tuple spelling and must not
            // close a modern prefixItems tuple whose items keyword is absent.
            (
                json!({"type": "array", "prefixItems": [{"type": "string"}], "additionalItems": false}),
                "[(string)?, ...Array<unknown>]",
            ),
        ] {
            assert_eq!(
                render_json_schema_to_typescript(&schema),
                expected,
                "{schema}"
            );
        }
    }

    #[test]
    fn objects_preserve_required_keys_and_do_not_contradict_named_properties() {
        for (schema, expected) in [
            (
                json!({"type": "object", "properties": {"z": {"type": "number"}}, "required": ["z", "a", "a"]}),
                "{ a: unknown; z: number; }",
            ),
            (
                json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"], "additionalProperties": {"type": "number"}}),
                "{ name: string; [key: string]: unknown; /* additional keys: number */ }",
            ),
            (
                json!({"type": "object", "additionalProperties": {"type": "number"}}),
                "{ [key: string]: number; }",
            ),
            (
                json!({"type": "object", "required": ["missing"], "additionalProperties": false}),
                "never",
            ),
            (
                json!({"type": "object", "properties": {"disabled": false}, "required": ["disabled"]}),
                "never",
            ),
            (
                json!({"type": "object", "properties": {"disabled": false}}),
                "{ disabled?: never; }",
            ),
            (
                json!({"type": "object", "required": ["path"], "additionalProperties": {"type": "string"}}),
                "{ path: string; [key: string]: unknown; /* additional keys: string */ }",
            ),
        ] {
            assert_eq!(
                render_json_schema_to_typescript(&schema),
                expected,
                "{schema}"
            );
        }
    }

    #[test]
    fn additional_key_annotation_cannot_close_its_own_comment() {
        assert_eq!(
            render_json_schema_to_typescript(&json!({
                "type": "object", "properties": {"id": {"type": "string"}},
                "additionalProperties": {"const": "*/ injected"}
            })),
            r#"{ id?: string; [key: string]: unknown; /* additional keys: "* / injected" */ }"#
        );
    }

    #[test]
    fn node_depth_and_byte_limits_each_stop_rendering_at_their_boundary() {
        let string = json!({"type": "string"});
        for (nodes, depth, bytes, expected) in [
            (1, 63, 6, Ok("string".to_string())),
            (0, 0, 100, Err(())),
            (10, 64, 100, Err(())),
            (10, 0, 5, Err(())),
        ] {
            let mut budget = RenderBudget {
                active_refs: HashSet::new(),
                nodes,
                depth,
                bytes,
            };
            assert_eq!(
                render_json_schema_to_typescript_inner(&string, &string, &mut budget),
                expected,
                "nodes={nodes}, depth={depth}, bytes={bytes}"
            );
        }
    }

    #[test]
    fn literal_byte_limit_includes_json_escaping_and_accepts_exact_fit() {
        let literal = json!("\n");
        for (bytes, expected) in [(4, Ok(r#""\n""#.to_string())), (3, Err(()))] {
            let mut budget = RenderBudget {
                bytes,
                ..RenderBudget::default()
            };
            assert_eq!(render_bounded_literal(&literal, &mut budget), expected);
        }
    }

    #[test]
    fn renders_json_strings_and_literals_without_serialization_recovery() {
        assert_eq!(
            render_json_schema_property_name("line\n\"item"),
            "\"line\\n\\\"item\""
        );
        assert_eq!(
            render_bounded_literal(&json!({"line": "one\ntwo"}), &mut RenderBudget::default())
                .unwrap(),
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
