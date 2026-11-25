use std::collections::HashSet;

use serde_json::Map;
use serde_json::Value;

/// The built-in OpenAI route rejects `oneOf` in strict output schemas. Lower only
/// the flat, disjoint tagged records supported here, not general JSON Schema.
pub(crate) fn lower_disjoint_output_records(schema: &mut Value) {
    // A JSON Pointer could reference the original /oneOf path.
    if contains_reference_keyword(schema) || closed_properties(schema).is_none() {
        return;
    }
    let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    for property in properties.values_mut() {
        let Some(union) = property.as_object_mut().filter(|union| union.len() == 1) else {
            continue;
        };
        let Some(branches) = union.get("oneOf").and_then(Value::as_array) else {
            continue;
        };
        let mut tags = HashSet::new();
        if branches.len() >= 2
            && branches
                .iter()
                .all(|branch| required_kind(branch).is_some_and(|tag| tags.insert(tag)))
            && let Some(branches) = union.remove("oneOf")
        {
            union.insert("anyOf".into(), branches);
        }
    }
}

fn contains_reference_keyword(value: &Value) -> bool {
    match value {
        Value::Object(values) => values
            .iter()
            .any(|(key, value)| key.starts_with('$') || contains_reference_keyword(value)),
        Value::Array(values) => values.iter().any(contains_reference_keyword),
        _ => false,
    }
}

fn closed_properties(schema: &Value) -> Option<&Map<String, Value>> {
    let object = schema.as_object()?;
    if object.len() != 4
        || object.get("type")?.as_str() != Some("object")
        || object.get("additionalProperties")?.as_bool() != Some(false)
    {
        return None;
    }
    let properties = object.get("properties")?.as_object()?;
    let required = object.get("required")?.as_array()?;
    if required.iter().any(|name| {
        !name
            .as_str()
            .is_some_and(|name| properties.contains_key(name))
    }) {
        return None;
    }
    Some(properties)
}

fn required_kind(schema: &Value) -> Option<&str> {
    let properties = closed_properties(schema)?;
    if !schema
        .get("required")?
        .as_array()?
        .iter()
        .any(|field| field.as_str() == Some("kind"))
    {
        return None;
    }
    let kind = properties.get("kind")?.as_object()?;
    if kind.len() != 2 || kind.get("type")?.as_str() != Some("string") {
        return None;
    }
    let [tag] = kind.get("enum")?.as_array()?.as_slice() else {
        return None;
    };
    tag.as_str()
}
