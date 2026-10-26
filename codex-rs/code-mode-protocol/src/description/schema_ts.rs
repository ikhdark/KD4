use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::io::Write;

use super::metadata::normalize_code_mode_identifier;

/// Smallest rendered shape worth replacing with a named alias. Below this the
/// alias name and its declaration cost more than the repetition they remove.
const FRAGMENT_HOIST_MIN_BYTES: usize = 160;

pub fn render_json_schema_to_typescript(schema: &JsonValue) -> String {
    render_json_schema_to_typescript_recording(schema).0
}

/// Renders the schema and reports the shapes rendered along the way.
///
/// A `$ref` target is re-rendered at every use site, and generated schemas also
/// inline the same shape at several sites without a reference. Reporting the
/// rendered shapes lets the declaration builder describe a repeated shape once
/// through [`hoist_shared_fragments`] instead of repeating it verbatim.
pub fn render_json_schema_to_typescript_recording(schema: &JsonValue) -> (String, SharedFragments) {
    let mut budget = RenderBudget::default();
    let rendered = render_json_schema_to_typescript_inner(schema, schema, &mut budget)
        .unwrap_or_else(|()| {
            budget.incomplete = true;
            "unknown /* schema projection incomplete: rendering limit reached */".to_string()
        });
    (
        rendered,
        SharedFragments {
            candidates: std::mem::take(&mut budget.fragments),
            incomplete: budget.incomplete,
        },
    )
}

/// Rendered shapes observed while projecting one schema, with any name hint
/// recovered from the reference that produced them.
#[derive(Default)]
pub struct SharedFragments {
    candidates: Vec<(Option<String>, String)>,
    pub incomplete: bool,
}

/// Replaces shapes that occur more than once across `types` with named aliases.
///
/// Returns alias declarations for the caller to emit before its tool contracts.
/// Recompute savings after each substitution so nested shapes can share aliases.
pub fn hoist_shared_fragments(
    fragments: &[&SharedFragments],
    types: &mut [&mut String],
) -> Vec<String> {
    let mut hints: BTreeMap<&str, Option<&str>> = BTreeMap::new();
    for fragment in fragments {
        for (hint, rendered) in &fragment.candidates {
            // Repeated numeric bounds can pay for an alias even when short.
            if rendered.len() < FRAGMENT_HOIST_MIN_BYTES
                && !rendered.starts_with("number /*")
                && !rendered.starts_with("(number | bigint) /*")
            {
                continue;
            }
            let slot = hints.entry(rendered.as_str()).or_default();
            if slot.is_none() {
                *slot = hint.as_deref();
            }
        }
    }
    let mut remaining: Vec<(String, Option<String>)> = hints
        .into_iter()
        .map(|(rendered, hint)| (rendered.to_string(), hint.map(str::to_string)))
        .collect();
    let mut used: HashSet<String> = HashSet::from(["CodeModeToolSearchResult".to_string()]);
    let mut declarations: Vec<(String, String)> = Vec::new();

    // Shapes nest, so the best choice is not the largest one: naming a small
    // shape used at four sites removes more text than naming the container that
    // holds it at two. Take the largest saving available, apply it everywhere,
    // then re-count, because that substitution changes what the rest are worth.
    loop {
        let best = remaining
            .iter()
            .enumerate()
            .filter_map(|(index, (rendered, hint))| {
                let occurrences: usize = types
                    .iter()
                    .map(|text| type_fragment_count(text, rendered))
                    .chain(
                        declarations
                            .iter()
                            .filter(|(_, body)| body != rendered)
                            .map(|(_, body)| type_fragment_count(body, rendered)),
                    )
                    .sum();
                let name = alias_name(hint.as_deref(), declarations.len(), &used);
                // Include every reference and the alias declaration itself.
                let removed = occurrences.saturating_sub(1) * rendered.len();
                let added = (occurrences + 1) * name.len() + 10;
                removed
                    .checked_sub(added)
                    .filter(|saving| *saving > 0)
                    .map(|saving| (saving, index, name))
            })
            .max_by_key(|(saving, _, _)| *saving);
        let Some((_, index, name)) = best else {
            break;
        };
        let (rendered, _) = remaining.swap_remove(index);
        used.insert(name.clone());
        for text in types.iter_mut() {
            **text = replace_type_fragment(text, &rendered, &name);
        }
        for (_, body) in declarations.iter_mut() {
            if body != &rendered {
                *body = replace_type_fragment(body, &rendered, &name);
            }
        }
        // A nested substitution changes every enclosing candidate too. Keeping
        // the original candidate text loses those later sharing opportunities.
        for (candidate, _) in &mut remaining {
            *candidate = replace_type_fragment(candidate, &rendered, &name);
        }
        declarations.push((name, rendered));
    }

    // Sharing a parent can reduce its children's reference counts. Inline any
    // aliases that no longer pay for themselves in the final graph.
    loop {
        let redundant = declarations.iter().position(|(name, body)| {
            let occurrences: usize = types
                .iter()
                .map(|text| type_fragment_count(text, name))
                .chain(
                    declarations
                        .iter()
                        .filter(|(other, _)| other != name)
                        .map(|(_, text)| type_fragment_count(text, name)),
                )
                .sum();
            occurrences * body.len() <= body.len() + (occurrences + 1) * name.len() + 10
        });
        let Some(index) = redundant else {
            break;
        };
        let (name, body) = declarations.remove(index);
        for text in types.iter_mut() {
            **text = replace_type_fragment(text, &name, &body);
        }
        for (_, text) in &mut declarations {
            *text = replace_type_fragment(text, &name, &body);
        }
    }

    declarations
        .into_iter()
        .map(|(name, body)| format!("type {name} = {body};"))
        .collect()
}

// Generated types can contain arbitrary schema descriptions and string literals.
// A repeated shape inside either is data, not a type reference to substitute.
fn type_fragment_count(text: &str, fragment: &str) -> usize {
    let mut count = 0;
    visit_type_fragment_offsets(text, fragment, |_| count += 1);
    count
}

fn type_fragment_offsets(text: &str, fragment: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    visit_type_fragment_offsets(text, fragment, |offset| offsets.push(offset));
    offsets
}

fn visit_type_fragment_offsets(text: &str, fragment: &str, mut visit: impl FnMut(usize)) {
    let bytes = text.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        let tail = &bytes[offset..];
        if tail.starts_with(b"//") {
            offset += tail
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap_or(tail.len());
        } else if tail.starts_with(b"/*") {
            offset += tail
                .windows(2)
                .position(|pair| pair == b"*/")
                .map_or(tail.len(), |end| end + 2);
        } else if tail.starts_with(fragment.as_bytes())
            && !(fragment.as_bytes().first().is_some_and(|byte| is_identifier_byte(*byte))
                && offset > 0 && is_identifier_byte(bytes[offset - 1]))
            && !(fragment.as_bytes().last().is_some_and(|byte| is_identifier_byte(*byte))
                && bytes.get(offset + fragment.len()).is_some_and(|byte| is_identifier_byte(*byte)))
            // A quoted property name is not a type use, even if the schema also
            // returns that same long string as an enum or constant.
            && !((fragment.starts_with('"') || fragment.bytes().all(is_identifier_byte))
                && {
                    let after = text[offset + fragment.len()..].trim_start();
                    after.starts_with(':') || after.starts_with("?:")
                })
        {
            visit(offset);
            offset += fragment.len();
        } else if matches!(bytes[offset], b'"' | b'\'' | b'`') {
            let quote = bytes[offset];
            offset += 1;
            while offset < bytes.len() {
                if bytes[offset] == b'\\' {
                    offset = (offset + 2).min(bytes.len());
                } else if bytes[offset] == quote {
                    offset += 1;
                    break;
                } else {
                    offset += 1;
                }
            }
        } else {
            offset += 1;
        }
    }
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn replace_type_fragment(text: &str, fragment: &str, alias: &str) -> String {
    let mut rendered = String::with_capacity(text.len());
    let mut start = 0;
    for offset in type_fragment_offsets(text, fragment) {
        rendered.push_str(&text[start..offset]);
        rendered.push_str(alias);
        start = offset + fragment.len();
    }
    rendered.push_str(&text[start..]);
    rendered
}

fn alias_name(hint: Option<&str>, index: usize, used: &HashSet<String>) -> String {
    let base = hint
        .map(normalize_code_mode_identifier)
        .filter(|hint| hint.chars().next().is_some_and(char::is_alphabetic))
        .map(|hint| {
            let mut chars = hint.chars();
            let first = chars.next().unwrap_or('T').to_ascii_uppercase();
            format!("CodeMode{first}{}", chars.as_str())
        })
        .unwrap_or_else(|| format!("CodeModeShape{}", index + 1));
    let mut name = base.clone();
    let mut suffix = 2;
    while used.contains(&name) {
        name = format!("{base}{suffix}");
        suffix += 1;
    }
    name
}

// Bound traversal before expanding references, and account for intermediate strings
// as well as the final output. A DAG can expand exponentially without any cycles.
struct RenderBudget {
    incomplete: bool,
    active_refs: HashSet<String>,
    fragments: Vec<(Option<String>, String)>,
    nodes: usize,
    depth: usize,
    bytes: usize,
}

impl Default for RenderBudget {
    fn default() -> Self {
        Self {
            incomplete: false,
            active_refs: HashSet::new(),
            fragments: Vec::new(),
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

const NESTED_RESOURCE_UNKNOWN: &str =
    "unknown /* schema projection incomplete: nested $id resource not projected */";

fn render_json_schema_to_typescript_inner(
    schema: &JsonValue,
    root: &JsonValue,
    budget: &mut RenderBudget,
) -> Rendered {
    render_schema_position(schema, root, budget, true)
}

fn render_schema_position(
    schema: &JsonValue,
    root: &JsonValue,
    budget: &mut RenderBudget,
    include_description: bool,
) -> Rendered {
    let available_bytes = budget.bytes;
    budget.nodes = budget.nodes.checked_sub(1).ok_or(())?;
    if budget.depth >= 64 {
        return Err(());
    }
    budget.depth += 1;
    let rendered = render_schema(schema, root, budget);
    budget.depth -= 1;
    let mut rendered = rendered?;
    if include_description
        && let Some(description) = schema.get("description").and_then(JsonValue::as_str)
        && !description.trim().is_empty()
    {
        let comment = format!("/* {} */", sanitize_comment(description.trim()));
        if !rendered.contains(&comment) {
            rendered = format!("{comment} {rendered}");
        }
    }
    // Children have already charged their rendered bytes. The enclosing shape
    // contains those same bytes; charge its final size once, not once per depth.
    budget.bytes = available_bytes.checked_sub(rendered.len()).ok_or(())?;
    if rendered.len() >= FRAGMENT_HOIST_MIN_BYTES || rendered.starts_with("number /*") {
        budget.fragments.push((None, rendered.clone()));
    }
    Ok(rendered)
}

fn render_schema(schema: &JsonValue, root: &JsonValue, budget: &mut RenderBudget) -> Rendered {
    // A nested resource has its own reference identity. Until resource resolution
    // is supported, never resolve its fragments against the enclosing document.
    if !std::ptr::eq(schema, root) && schema.get("$id").and_then(JsonValue::as_str).is_some() {
        budget.incomplete = true;
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
                    // Include validation-only siblings too: a referenced type
                    // need not be restated beside its bounds. Otherwise those
                    // constraints silently bypass authoritative-schema recovery.
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
                                | "minimum"
                                | "maximum"
                                | "exclusiveMinimum"
                                | "exclusiveMaximum"
                                | "multipleOf"
                                | "pattern"
                                | "minLength"
                                | "maxLength"
                                | "format"
                                | "minItems"
                                | "maxItems"
                                | "additionalItems"
                                | "not"
                                | "if"
                                | "then"
                                | "else"
                                | "$dynamicRef"
                                | "$recursiveRef"
                                | "uniqueItems"
                                | "contains"
                                | "minContains"
                                | "maxContains"
                                | "unevaluatedItems"
                                | "minProperties"
                                | "maxProperties"
                                | "patternProperties"
                                | "propertyNames"
                                | "unevaluatedProperties"
                                | "dependentRequired"
                                | "dependentSchemas"
                                | "dependencies"
                        )
                    });
                    return Ok(if dialect.is_empty() && has_siblings {
                        budget.incomplete = true;
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
                        "string | (number | bigint) | boolean | null | {array} | {object}"
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
        budget.incomplete = true;
        return Ok("unknown /* external $ref not projected */".to_string());
    };
    let Some(pointer) = decode_schema_fragment(pointer) else {
        budget.incomplete = true;
        return Ok("unknown /* invalid $ref URI fragment */".to_string());
    };
    if !pointer.is_empty() && !pointer.starts_with('/') {
        budget.incomplete = true;
        return Ok("unknown /* unresolved $ref */".to_string());
    }
    let mut target = root;
    // Walk one escaped token at a time so pointers cannot skip over a resource
    // boundary into a descendant that no longer contains the ancestor's $id.
    for token in pointer.split('/').skip(1) {
        budget.nodes = budget.nodes.checked_sub(1).ok_or(())?;
        if !std::ptr::eq(target, root) && target.get("$id").and_then(JsonValue::as_str).is_some() {
            budget.incomplete = true;
            return Ok(NESTED_RESOURCE_UNKNOWN.to_string());
        }
        let Some(next) = target.pointer(&format!("/{token}")) else {
            budget.incomplete = true;
            return Ok("unknown /* unresolved $ref */".to_string());
        };
        target = next;
    }
    if !budget.active_refs.insert(pointer.clone()) {
        budget.incomplete = true;
        return Ok("unknown /* recursive $ref */".to_string());
    }
    let rendered = render_json_schema_to_typescript_inner(target, root, budget);
    budget.active_refs.remove(&pointer);
    // The reference name is the only human-meaningful label available for a
    // shape that will be described once and used from several sites.
    if let Ok(rendered) = &rendered
        && rendered.len() >= FRAGMENT_HOIST_MIN_BYTES
        && let Some(name) = pointer.rsplit('/').next().filter(|name| !name.is_empty())
    {
        budget
            .fragments
            .push((Some(name.to_string()), rendered.clone()));
    }
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
    if let Some(value) = map.get("default") {
        let value = sanitize_comment(&render_bounded_literal(value, budget)?);
        annotations.push(format!(
            "default annotation: {value}; not automatically applied on omission"
        ));
    }
    if !map.contains_key("type") {
        for keyword in [
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
        ] {
            if let Some(value) = map.get(keyword) {
                let value = sanitize_comment(&render_bounded_literal(value, budget)?);
                annotations.push(format!("{keyword} (numbers only): {value}"));
            }
        }
    }
    if map.contains_key("oneOf") {
        annotations.push("oneOf: exactly one branch must match".to_string());
    }
    // These constraints are not expressible by the current structural
    // projection. Preserve the authoritative schema through the existing
    // incomplete-projection recovery path rather than silently dropping them.
    for keyword in [
        "not",
        "if",
        "then",
        "else",
        "$dynamicRef",
        "$recursiveRef",
        "uniqueItems",
        "contains",
        "minContains",
        "maxContains",
        "unevaluatedItems",
        "minProperties",
        "maxProperties",
        "propertyNames",
        "unevaluatedProperties",
        "dependentRequired",
        "dependentSchemas",
        "dependencies",
    ] {
        if map.contains_key(keyword) {
            budget.incomplete = true;
            annotations.push(format!("unprojected keyword: {keyword}"));
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
    let safe = map
        .get("minimum")
        .or_else(|| map.get("exclusiveMinimum"))
        .and_then(JsonValue::as_f64)
        .is_some_and(|bound| bound >= -9_007_199_254_740_991.0)
        && map
            .get("maximum")
            .or_else(|| map.get("exclusiveMaximum"))
            .and_then(JsonValue::as_f64)
            .is_some_and(|bound| bound <= 9_007_199_254_740_991.0);
    // `number` includes integral JSON lexemes too. The lossless decoder uses
    // BigInt for unsafe integers regardless of the provider's schema label.
    let numeric_type = if !safe {
        "(number | bigint)"
    } else {
        "number"
    };
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
        numeric_type.to_string()
    } else {
        format!("{numeric_type} /* {} */", constraints.join("; "))
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
    // Draft-06/07 and 2019-09 apply schema-valued `items` to every element. Their
    // unknown `prefixItems` must not turn that constraint into a tail-only rule.
    let legacy_items = root
        .get("$schema")
        .and_then(JsonValue::as_str)
        .is_some_and(|dialect| {
            matches!(
                dialect.trim_end_matches('#'),
                "http://json-schema.org/draft-06/schema"
                    | "https://json-schema.org/draft-06/schema"
                    | "http://json-schema.org/draft-07/schema"
                    | "https://json-schema.org/draft-07/schema"
                    | "http://json-schema.org/draft/2019-09/schema"
                    | "https://json-schema.org/draft/2019-09/schema"
            )
        });
    let has_prefix_items = !legacy_items && map.contains_key("prefixItems");
    // `items: [...]` is the tuple spelling used by older schema drafts.
    let prefix = has_prefix_items
        .then(|| map.get("prefixItems"))
        .flatten()
        .and_then(JsonValue::as_array)
        .or_else(|| map.get("items").and_then(JsonValue::as_array));
    let trailing = if has_prefix_items {
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
                .split(['\n', '\r', '\u{2028}', '\u{2029}'])
                .map(str::trim)
                .filter(|line| !line.is_empty())
            {
                lines.push(format!("// {description_line}"));
            }
        }
        budget.spend(name.len().saturating_mul(6))?;
        let property_name = render_json_schema_property_name(name);
        let mut property_type = render_schema_position(value, root, budget, false)?;
        // A reference can repeat the property's own guidance. Keep the existing
        // property comment without duplicating that same annotation at its type.
        if let Some(description) = value.get("description").and_then(JsonValue::as_str) {
            let comment = format!("/* {} */ ", sanitize_comment(description.trim()));
            if let Some(rest) = property_type.strip_prefix(&comment) {
                property_type = rest.to_string();
            }
        }
        let optional = if required.contains(name) { "" } else { "?" };
        lines.push(format!("{property_name}{optional}: {property_type};"));
    }
    if has_patterns {
        budget.incomplete = true;
        lines.push("[key: string]: unknown; /* patternProperties not projected */".to_string());
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
    write_typescript_literal(value, &mut writer).map_err(|_| ())?;
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
        JsonValue::Object(values) => {
            // Structural TS object types cannot enforce all constant-object
            // semantics (notably extra keys). Keep the authoritative schema.
            budget.incomplete = true;
            values.values().try_for_each(|value| bound_literal_traversal(value, budget))
        }
        _ => Ok(()),
    };
    budget.depth -= 1;
    result
}

struct BoundedLiteralWriter {
    bytes: Vec<u8>,
    limit: usize,
}

fn write_typescript_literal(value: &JsonValue, writer: &mut BoundedLiteralWriter) -> std::io::Result<()> {
    match value {
        JsonValue::Number(number) if number.as_i64().is_some_and(|number| number.unsigned_abs() > 9_007_199_254_740_991)
            || number.as_u64().is_some_and(|number| number > 9_007_199_254_740_991) => write!(writer, "{number}n"),
        JsonValue::Array(values) => {
            writer.write_all(b"[")?;
            for (index, value) in values.iter().enumerate() {
                if index != 0 { writer.write_all(b",")?; }
                write_typescript_literal(value, writer)?;
            }
            writer.write_all(b"]")
        }
        JsonValue::Object(values) => {
            if values.is_empty() { return writer.write_all(b"Record<string, never>"); }
            writer.write_all(b"{")?;
            for (index, (key, value)) in values.iter().enumerate() {
                if index != 0 { writer.write_all(b",")?; }
                serde_json::to_writer(&mut *writer, key).map_err(std::io::Error::other)?;
                writer.write_all(b":")?;
                write_typescript_literal(value, writer)?;
            }
            writer.write_all(b"}")
        }
        _ => serde_json::to_writer(writer, value).map_err(std::io::Error::other),
    }
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
