//! JSON schema normalization, experimental filtering, and canonical serialization.

use super::GeneratedSchema;
use super::JSON_V1_ALLOWLIST;
use super::ensure_dir;
use super::generate_json_schema;
use super::remove_generated_type_files;
use anyhow::Context;
use anyhow::Result;
use codex_app_server_protocol_types as types;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use types::schema_support::RawSchemaEntry;
use types::schema_support::SERVER_NOTIFICATION_METADATA;

pub(super) const IGNORED_DEFINITIONS: &[&str] = &["Option<()>"];
const SCHEMA_MAP_KEYWORDS: &[&str] = &[
    "properties", "patternProperties", "dependentSchemas", "dependencies",
];
const SCHEMA_VALUE_KEYWORDS: &[&str] = &[
    "items", "additionalItems", "additionalProperties", "contains", "propertyNames",
    "not", "if", "then", "else", "unevaluatedProperties", "unevaluatedItems",
    "contentSchema", "schema", "allOf", "anyOf", "oneOf", "prefixItems",
];

/// Remove non-validation titles only from schema nodes for equivalence checks.
/// Literal instance values in defaults, examples, const and enum stay untouched.
pub(super) fn remove_schema_titles(value: &mut Value) {
    let Some(map) = value.as_object_mut() else { return; };
    map.remove("title");
    for keyword in SCHEMA_MAP_KEYWORDS.iter().copied().chain(["definitions", "$defs"]) {
        if let Some(children) = map.get_mut(keyword).and_then(Value::as_object_mut) {
            for child in children.values_mut() { remove_schema_titles(child); }
        }
    }
    for keyword in SCHEMA_VALUE_KEYWORDS {
        if let Some(child) = map.get_mut(*keyword) {
            if let Some(children) = child.as_array_mut() {
                for child in children { remove_schema_titles(child); }
            } else { remove_schema_titles(child); }
        }
    }
}

/// Visit only schema-valued keywords; annotation values are opaque instances.
pub(super) fn for_each_schema_child(value: &Value, include_definitions: bool, visit: &mut impl FnMut(&Value)) {
    let Some(map) = value.as_object() else { return; };
    for key in SCHEMA_MAP_KEYWORDS {
        if let Some(children) = map.get(*key).and_then(Value::as_object) {
            for child in children.values() { visit(child); }
        }
    }
    for key in SCHEMA_VALUE_KEYWORDS {
        if let Some(child) = map.get(*key) {
            if let Some(children) = child.as_array() {
                for child in children { visit(child); }
            } else { visit(child); }
        }
    }
    if include_definitions {
        for key in ["definitions", "$defs"] {
            if let Some(children) = map.get(key).and_then(Value::as_object) {
                for (name, child) in children {
                    if name == "v2" {
                        if let Some(namespace) = child.as_object() {
                            for child in namespace.values() { visit(child); }
                        }
                    } else { visit(child); }
                }
            }
        }
    }
}

pub(super) fn for_each_schema_child_mut(value: &mut Value, include_definitions: bool, visit: &mut impl FnMut(&mut Value)) {
    let Some(map) = value.as_object_mut() else { return; };
    for key in SCHEMA_MAP_KEYWORDS {
        if let Some(children) = map.get_mut(*key).and_then(Value::as_object_mut) {
            for child in children.values_mut() { visit(child); }
        }
    }
    for key in SCHEMA_VALUE_KEYWORDS {
        if let Some(child) = map.get_mut(*key) {
            if let Some(children) = child.as_array_mut() {
                for child in children { visit(child); }
            } else { visit(child); }
        }
    }
    if include_definitions {
        for key in ["definitions", "$defs"] {
            if let Some(children) = map.get_mut(key).and_then(Value::as_object_mut) {
                for (name, child) in children {
                    if name == "v2" {
                        if let Some(namespace) = child.as_object_mut() {
                            for child in namespace.values_mut() { visit(child); }
                        }
                    } else { visit(child); }
                }
            }
        }
    }
}

/// Decode the owning definition token, retaining any pointer tail for full validation.
pub(super) fn local_definition_name(reference: &str, definitions_key: &str) -> Option<String> {
    let suffix = reference.strip_prefix(&format!("#/{definitions_key}/"))?;
    let mut token = suffix.split('/').next()?.chars();
    let mut decoded = String::new();
    while let Some(ch) = token.next() {
        if ch == '~' {
            decoded.push(match token.next()? { '0' => '~', '1' => '/', _ => return None });
        } else { decoded.push(ch); }
    }
    Some(decoded)
}
const V1_CLIENT_REQUEST_METHODS: &[&str] =
    &["getConversationSummary", "gitDiffToRemote", "getAuthStatus"];

pub(super) fn filter_experimental_schema_with_metadata(
    bundle: &mut Value,
    registered_fields: &[&'static types::ExperimentalField],
    methods: &HashSet<&str>,
    method_types: &HashSet<String>,
) {
    filter_experimental_fields_in_root(bundle, registered_fields);
    filter_experimental_fields_in_definitions(bundle, registered_fields);
    prune_experimental_methods_inner(bundle, methods);
    if let Some(definitions) = bundle.get_mut("definitions").and_then(Value::as_object_mut) {
        remove_experimental_method_type_definitions_map(definitions, method_types);
    }
}

fn filter_experimental_fields_in_root(
    schema: &mut Value,
    experimental_fields: &[&'static types::ExperimentalField],
) {
    let Some(title) = schema.get("title").and_then(Value::as_str) else {
        return;
    };
    let title = title.to_string();

    for field in experimental_fields {
        if title != field.type_name {
            continue;
        }
        remove_property_from_schema(schema, field.field_name);
    }
}

fn filter_experimental_fields_in_definitions(
    bundle: &mut Value,
    experimental_fields: &[&'static types::ExperimentalField],
) {
    let Some(definitions) = bundle.get_mut("definitions").and_then(Value::as_object_mut) else {
        return;
    };

    filter_experimental_fields_in_definitions_map(definitions, experimental_fields);
}

fn filter_experimental_fields_in_definitions_map(
    definitions: &mut Map<String, Value>,
    experimental_fields: &[&'static types::ExperimentalField],
) {
    for (def_name, def_schema) in definitions.iter_mut() {
        if def_name == "v2" {
            if let Some(namespace_defs) = def_schema.as_object_mut() {
                filter_experimental_fields_in_definitions_map(namespace_defs, experimental_fields);
            }
            continue;
        }

        for field in experimental_fields {
            if !definition_matches_type(def_name, field.type_name) {
                continue;
            }
            remove_property_from_schema(def_schema, field.field_name);
        }
    }
}


fn definition_matches_type(def_name: &str, type_name: &str) -> bool {
    def_name == type_name || def_name.ends_with(&format!("::{type_name}"))
}

fn remove_property_from_schema(schema: &mut Value, field_name: &str) {
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.remove(field_name);
    }

    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|entry| entry.as_str() != Some(field_name));
    }

    if let Some(inner_schema) = schema.get_mut("schema") {
        remove_property_from_schema(inner_schema, field_name);
    }
    // Composition branches describe this same owning object, unlike nested
    // properties whose fields belong to different types.
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = schema.get_mut(keyword).and_then(Value::as_array_mut) {
            for branch in branches {
                remove_property_from_schema(branch, field_name);
            }
        }
    }
}

fn prune_experimental_methods_inner(value: &mut Value, experimental_methods: &HashSet<&str>) {
    for keyword in ["oneOf", "anyOf", "allOf"] {
        if let Some(items) = value.get_mut(keyword).and_then(Value::as_array_mut) {
            items.retain(|item| !is_experimental_method_variant(item, experimental_methods));
        }
    }
    for_each_schema_child_mut(value, true, &mut |child| prune_experimental_methods_inner(child, experimental_methods));
}

fn is_experimental_method_variant(value: &Value, experimental_methods: &HashSet<&str>) -> bool {
    let Value::Object(map) = value else {
        return false;
    };
    let Some(properties) = map.get("properties").and_then(Value::as_object) else {
        return false;
    };
    let Some(method_schema) = properties.get("method").and_then(Value::as_object) else {
        return false;
    };

    if let Some(method) = method_schema.get("const").and_then(Value::as_str) {
        return experimental_methods.contains(method);
    }

    if let Some(values) = method_schema.get("enum").and_then(Value::as_array)
        && values.len() == 1
        && let Some(method) = values[0].as_str()
    {
        return experimental_methods.contains(method);
    }

    false
}

pub(super) fn filter_experimental_json_files(
    out_dir: &Path,
    generated_paths: &HashSet<PathBuf>,
    registered_fields: &[&'static types::ExperimentalField],
    methods: &HashSet<&str>,
    method_types: &HashSet<String>,
) -> Result<()> {
    for path in json_files_in_recursive(out_dir)? {
        if path
            .strip_prefix(out_dir)
            .is_ok_and(|relative| generated_paths.contains(relative))
            || path == out_dir.join("codex_app_server_protocol.schemas.json")
            || path == out_dir.join("codex_app_server_protocol.v2.schemas.json")
        {
            continue;
        }
        let mut value = read_json_value(&path)?;
        filter_experimental_schema_with_metadata(
            &mut value,
            registered_fields,
            methods,
            method_types,
        );
        write_pretty_json(path, &value)?;
    }
    remove_generated_type_files(out_dir, method_types, "json")?;
    Ok(())
}

fn remove_experimental_method_type_definitions_map(
    definitions: &mut Map<String, Value>,
    experimental_type_names: &HashSet<String>,
) {
    let keys_to_remove: Vec<String> = definitions
        .keys()
        .filter(|def_name| {
            experimental_type_names
                .iter()
                .any(|type_name| definition_matches_type(def_name, type_name))
        })
        .cloned()
        .collect();
    for key in keys_to_remove {
        definitions.remove(&key);
    }

    for (name, value) in definitions.iter_mut() {
        if name != "v2" {
            continue;
        }
        if let Some(namespace_defs) = value.as_object_mut() {
            remove_experimental_method_type_definitions_map(
                namespace_defs,
                experimental_type_names,
            );
        }
    }
}

fn json_files_in_recursive(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            anyhow::ensure!(!kind.is_symlink(), "linked JSON schema output is not supported: {}", path.display());
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            if matches!(path.extension().and_then(|ext| ext.to_str()), Some("json")) {
                out.push(path);
            }
        }
    }
    Ok(out)
}

fn read_json_value(path: &Path) -> Result<Value> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))
}

pub(super) fn normalize_raw_schemas(schemas: Vec<RawSchemaEntry>) -> Result<Vec<GeneratedSchema>> {
    schemas
        .into_iter()
        .map(|entry| normalize_json_schema(entry.name, entry.schema))
        .collect()
}

pub(super) fn normalize_json_schema(name: &str, mut schema_value: Value) -> Result<GeneratedSchema> {
    let file_stem = name.trim();
    let (raw_namespace, logical_name) = split_namespace(file_stem);
    let include_in_json_codegen =
        raw_namespace != Some("v1") || JSON_V1_ALLOWLIST.contains(&logical_name);
    if include_in_json_codegen {
        if file_stem == "ClientRequest" {
            strip_v1_client_request_variants_from_json_schema(&mut schema_value);
        } else if file_stem == "ServerNotification" {
            strip_v1_server_notification_variants_from_json_schema(&mut schema_value);
        }
        if let Some(Value::Object(error_schema)) =
            schema_value.pointer_mut("/definitions/CodexErrorInfo")
            // Internal RolloutLine schemas use core's strict snake_case enum
            // with the same schema name. Only the app-server wire enum is open.
            && error_schema.get("oneOf").and_then(Value::as_array).is_some_and(|variants| {
                variants.iter().any(|variant| {
                    variant.get("enum").and_then(Value::as_array).is_some_and(|tags| {
                        tags.iter().any(|tag| tag.as_str() == Some("contextWindowExceeded"))
                    })
                })
            })
            && let Some(Value::Array(mut variants)) = error_schema.remove("oneOf")
        {
            let known_tags: Vec<Value> = variants.iter().flat_map(|variant| {
                variant.get("enum").and_then(Value::as_array).cloned()
                    .unwrap_or_else(|| variant.get("properties").and_then(Value::as_object)
                        .map(|properties| properties.keys().cloned().map(Value::String).collect())
                        .unwrap_or_default())
            }).collect();
            variants.push(serde_json::json!({
                "type": "string", "not": { "enum": known_tags }
            }));
            variants.push(serde_json::json!({
                "type": "object", "minProperties": 1, "maxProperties": 1,
                "propertyNames": { "not": { "enum": known_tags } }
            }));
            error_schema.insert("anyOf".to_string(), Value::Array(variants));
        }
        enforce_numbered_definition_collision_overrides(file_stem, &mut schema_value);
        annotate_schema(&mut schema_value, Some(file_stem));
    }
    let namespace = match raw_namespace {
        Some("v1") | None => None,
        Some(ns) => Some(ns.to_string()),
    };
    Ok(GeneratedSchema {
        in_v1_dir: raw_namespace == Some("v1"),
        namespace,
        logical_name: logical_name.to_string(),
        value: schema_value,
    })
}

pub(super) fn json_schema_relative_path(schema: &GeneratedSchema) -> Option<PathBuf> {
    if IGNORED_DEFINITIONS.contains(&schema.logical_name.as_str())
        || (schema.in_v1_dir && !JSON_V1_ALLOWLIST.contains(&schema.logical_name.as_str()))
    {
        return None;
    }
    let namespace = if schema.in_v1_dir {
        "v1"
    } else {
        schema.namespace.as_deref().unwrap_or("")
    };
    Some(Path::new(namespace).join(format!("{}.json", schema.logical_name)))
}

pub(super) fn write_json_schema<T: JsonSchema>(out_dir: &Path, name: &str) -> Result<()> {
    let schema = generate_json_schema::<T>(name)?;
    if let Some(relative_path) = json_schema_relative_path(&schema) {
        let path = out_dir.join(relative_path);
        if let Some(parent) = path.parent() {
            ensure_dir(parent)?;
        }
        write_pretty_json(path, &schema.value)?;
    }
    Ok(())
}

fn enforce_numbered_definition_collision_overrides(schema_name: &str, schema: &mut Value) {
    for defs_key in ["definitions", "$defs"] {
        let Some(defs) = schema.get(defs_key).and_then(Value::as_object) else {
            continue;
        };
        detect_numbered_definition_collisions(schema_name, defs_key, defs);
    }
}

fn strip_v1_client_request_variants_from_json_schema(schema: &mut Value) {
    let v1_methods: HashSet<&str> = V1_CLIENT_REQUEST_METHODS.iter().copied().collect();
    strip_method_variants_from_json_schema(schema, &v1_methods);
}

fn strip_v1_server_notification_variants_from_json_schema(schema: &mut Value) {
    let methods: HashSet<&str> = SERVER_NOTIFICATION_METADATA
        .iter()
        .filter(|metadata| metadata.json_export_excluded)
        .map(|metadata| metadata.method)
        .collect();
    strip_method_variants_from_json_schema(schema, &methods);
}

fn strip_method_variants_from_json_schema(schema: &mut Value, methods_to_remove: &HashSet<&str>) {
    {
        let Some(root) = schema.as_object_mut() else {
            return;
        };
        let Some(Value::Array(variants)) = root.get_mut("oneOf") else {
            return;
        };
        variants.retain(|variant| !is_method_variant_in_set(variant, methods_to_remove));
    }

    let reachable = reachable_local_definitions(schema, "definitions");
    let Some(root) = schema.as_object_mut() else {
        return;
    };
    if let Some(definitions) = root.get_mut("definitions").and_then(Value::as_object_mut) {
        definitions.retain(|name, _| reachable.contains(name));
    }
}

fn is_method_variant_in_set(value: &Value, methods: &HashSet<&str>) -> bool {
    let Value::Object(map) = value else {
        return false;
    };
    let Some(properties) = map.get("properties").and_then(Value::as_object) else {
        return false;
    };
    let Some(method_schema) = properties.get("method") else {
        return false;
    };
    let Some(method) = string_literal(method_schema) else {
        return false;
    };
    methods.contains(method)
}

fn reachable_local_definitions(schema: &Value, defs_key: &str) -> HashSet<String> {
    let Some(definitions) = schema.get(defs_key).and_then(Value::as_object) else {
        return HashSet::new();
    };
    let mut queue: Vec<String> = Vec::new();
    let mut reachable: HashSet<String> = HashSet::new();

    collect_local_definition_refs_excluding_maps(schema, defs_key, &mut queue, &mut reachable);

    while let Some(name) = queue.pop() {
        if let Some(def_schema) = definitions.get(&name) {
            collect_local_definition_refs(def_schema, defs_key, &mut queue, &mut reachable);
        }
    }
    reachable
}

fn collect_local_definition_refs_excluding_maps(value: &Value, defs_key: &str, queue: &mut Vec<String>, reachable: &mut HashSet<String>) {
    collect_local_definition_ref_here(value, defs_key, queue, reachable);
    for_each_schema_child(value, false, &mut |child| collect_local_definition_refs_excluding_maps(child, defs_key, queue, reachable));
}

fn collect_local_definition_refs(value: &Value, defs_key: &str, queue: &mut Vec<String>, reachable: &mut HashSet<String>) {
    collect_local_definition_ref_here(value, defs_key, queue, reachable);
    for_each_schema_child(value, true, &mut |child| collect_local_definition_refs(child, defs_key, queue, reachable));
}

fn collect_local_definition_ref_here(value: &Value, defs_key: &str, queue: &mut Vec<String>, reachable: &mut HashSet<String>) {
    let Some(name) = value.get("$ref").and_then(Value::as_str).and_then(|reference| local_definition_name(reference, defs_key)) else { return; };
    if reachable.insert(name.clone()) { queue.push(name); }
}

fn detect_numbered_definition_collisions(
    schema_name: &str,
    defs_key: &str,
    defs: &Map<String, Value>,
) {
    for generated_name in defs.keys() {
        let base_name = generated_name.trim_end_matches(|c: char| c.is_ascii_digit());
        if base_name == generated_name || !defs.contains_key(base_name) {
            continue;
        }

        panic!(
            "Numbered definition naming collision detected: schema={schema_name}|container={defs_key}|generated={generated_name}|base={base_name}"
        );
    }
}

fn write_pretty_json(path: PathBuf, value: &impl Serialize) -> Result<()> {
    let json = pretty_json_bytes(value)
        .with_context(|| format!("Failed to serialize JSON schema to {}", path.display()))?;
    fs::write(&path, json).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

fn pretty_json_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    pretty_json_value_bytes(serde_json::to_value(value)?)
}

pub(super) fn pretty_json_value_bytes(mut value: Value) -> Result<Vec<u8>> {
    // Workspace feature unification can enable serde_json/preserve_order.
    // Fixtures must retain the same canonical ordering in either build.
    value.sort_all_objects();
    Ok(serde_json::to_vec_pretty(&value)?)
}

/// Split a fully-qualified type name like "v2::Type" into its namespace and logical name.
fn split_namespace(name: &str) -> (Option<&str>, &str) {
    name.split_once("::")
        .map_or((None, name), |(ns, rest)| (Some(ns), rest))
}

fn variant_definition_name(base: &str, variant: &Value) -> Option<String> {
    if let Some(props) = variant.get("properties").and_then(Value::as_object) {
        if let Some(method_literal) = literal_from_property(props, "method") {
            let pascal = to_pascal_case(method_literal);
            return Some(match base {
                "ClientRequest" | "ServerRequest" => format!("{pascal}Request"),
                "ClientNotification" | "ServerNotification" => format!("{pascal}Notification"),
                _ => format!("{pascal}{base}"),
            });
        }

        if let Some(type_literal) = literal_from_property(props, "type") {
            let pascal = to_pascal_case(type_literal);
            return Some(match base {
                "EventMsg" => format!("{pascal}EventMsg"),
                _ => format!("{pascal}{base}"),
            });
        }

        if props.len() == 1
            && let Some(key) = props.keys().next()
        {
            let pascal = props
                .get(key)
                .and_then(string_literal)
                .map(to_pascal_case)
                .unwrap_or_else(|| to_pascal_case(key));
            return Some(format!("{pascal}{base}"));
        }
    }

    if let Some(required) = variant.get("required").and_then(Value::as_array)
        && required.len() == 1
        && let Some(key) = required[0].as_str()
    {
        let pascal = to_pascal_case(key);
        return Some(format!("{pascal}{base}"));
    }

    None
}

fn literal_from_property<'a>(props: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    props.get(key).and_then(string_literal)
}

fn string_literal(value: &Value) -> Option<&str> {
    value.get("const").and_then(Value::as_str).or_else(|| {
        value
            .get("enum")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(Value::as_str)
    })
}

pub(super) fn annotate_schema(value: &mut Value, base: Option<&str>) {
    match value {
        Value::Object(map) => annotate_object(map, base),
        Value::Array(items) => {
            for item in items {
                annotate_schema(item, base);
            }
        }
        _ => {}
    }
}

fn annotate_object(map: &mut Map<String, Value>, base: Option<&str>) {
    let owner = map.get("title").and_then(Value::as_str).map(str::to_owned);
    if let Some(owner) = owner.as_deref()
        && let Some(Value::Object(props)) = map.get_mut("properties")
    {
        set_discriminator_titles(props, owner);
    }

    if let Some(Value::Array(variants)) = map.get_mut("oneOf") {
        annotate_variant_list(variants, base);
    }
    if let Some(Value::Array(variants)) = map.get_mut("anyOf") {
        annotate_variant_list(variants, base);
    }

    if let Some(Value::Object(defs)) = map.get_mut("definitions") {
        for (name, schema) in defs.iter_mut() {
            annotate_schema(schema, Some(name.as_str()));
        }
    }

    if let Some(Value::Object(defs)) = map.get_mut("$defs") {
        for (name, schema) in defs.iter_mut() {
            annotate_schema(schema, Some(name.as_str()));
        }
    }

    if let Some(Value::Object(props)) = map.get_mut("properties") {
        for value in props.values_mut() {
            annotate_schema(value, base);
        }
    }

    if let Some(items) = map.get_mut("items") {
        annotate_schema(items, base);
    }

    if let Some(additional) = map.get_mut("additionalProperties") {
        annotate_schema(additional, base);
    }

    for (key, child) in map.iter_mut() {
        match key.as_str() {
            "oneOf"
            | "anyOf"
            | "definitions"
            | "$defs"
            | "properties"
            | "items"
            | "additionalProperties" => {}
            "patternProperties" | "dependentSchemas" | "dependencies" => {
                if let Some(schemas) = child.as_object_mut() {
                    for schema in schemas.values_mut() { annotate_schema(schema, base); }
                }
            }
            "allOf" | "not" | "if" | "then" | "else" | "contains" | "propertyNames"
            | "additionalItems" | "unevaluatedProperties" | "unevaluatedItems" | "prefixItems"
            | "contentSchema" | "schema" => annotate_schema(child, base),
            _ => {}
        }
    }
}

fn annotate_variant_list(variants: &mut [Value], base: Option<&str>) {
    let mut seen = HashSet::new();

    for variant in variants.iter() {
        if let Some(name) = variant_title(variant) {
            seen.insert(name.to_owned());
        }
    }

    for variant in variants.iter_mut() {
        let mut variant_name = variant_title(variant).map(str::to_owned);

        if variant_name.is_none()
            && let Some(base_name) = base
            && let Some(name) = variant_definition_name(base_name, variant)
        {
            let candidate = name.clone();
            if seen.contains(&candidate) {
                let collision_key = variant_title_collision_key(base_name, &name, variant);
                panic!(
                    "Variant title naming collision detected: {collision_key} (generated name: {name})"
                );
            }
            if let Some(obj) = variant.as_object_mut() {
                obj.insert("title".into(), Value::String(candidate.clone()));
            }
            seen.insert(candidate.clone());
            variant_name = Some(candidate);
        }

        if let Some(name) = variant_name.as_deref()
            && let Some(obj) = variant.as_object_mut()
            && let Some(Value::Object(props)) = obj.get_mut("properties")
        {
            set_discriminator_titles(props, name);
        }

        annotate_schema(variant, base);
    }
}

fn variant_title_collision_key(base: &str, generated_name: &str, variant: &Value) -> String {
    let mut parts = vec![
        format!("base={base}"),
        format!("generated={generated_name}"),
    ];

    if let Some(props) = variant.get("properties").and_then(Value::as_object) {
        for key in DISCRIMINATOR_KEYS {
            if let Some(value) = literal_from_property(props, key) {
                parts.push(format!("{key}={value}"));
            }
        }
        for (key, value) in props {
            if DISCRIMINATOR_KEYS.contains(&key.as_str()) {
                continue;
            }
            if let Some(literal) = string_literal(value) {
                parts.push(format!("literal:{key}={literal}"));
            }
        }

        if props.len() == 1
            && let Some(key) = props.keys().next()
        {
            parts.push(format!("only_property={key}"));
        }
    }

    if let Some(required) = variant.get("required").and_then(Value::as_array)
        && required.len() == 1
        && let Some(key) = required[0].as_str()
    {
        parts.push(format!("required_only={key}"));
    }

    if parts.len() == 2 {
        parts.push(format!("variant={variant}"));
    }

    parts.join("|")
}

const DISCRIMINATOR_KEYS: &[&str] = &["type", "method", "mode", "status", "role", "reason"];

fn set_discriminator_titles(props: &mut Map<String, Value>, owner: &str) {
    for key in DISCRIMINATOR_KEYS {
        if let Some(prop_schema) = props.get_mut(*key)
            && string_literal(prop_schema).is_some()
            && let Value::Object(prop_obj) = prop_schema
        {
            if prop_obj.contains_key("title") {
                continue;
            }
            let suffix = to_pascal_case(key);
            prop_obj.insert("title".into(), Value::String(format!("{owner}{suffix}")));
        }
    }
}

fn variant_title(value: &Value) -> Option<&str> {
    value
        .as_object()
        .and_then(|obj| obj.get("title"))
        .and_then(Value::as_str)
}

fn to_pascal_case(input: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = true;

    for c in input.chars() {
        if c == '_' || c == '-' {
            capitalize_next = true;
            continue;
        }

        if capitalize_next {
            result.extend(c.to_uppercase());
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }

    result
}

