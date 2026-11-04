//! Mixed-namespace and flat-v2 schema bundles and their reference closure.

use super::GeneratedSchema;
use super::json_schema::IGNORED_DEFINITIONS;
use super::json_schema::annotate_schema;
use super::json_schema::remove_schema_titles;
use anyhow::Result;
use anyhow::anyhow;
use serde_json::Map;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;

const SPECIAL_DEFINITIONS: &[&str] = &[
    "ClientNotification",
    "ClientRequest",
    "ServerNotification",
    "ServerRequest",
];
const FLAT_V2_SHARED_DEFINITIONS: &[&str] =
    &["ClientRequest", "OverloadErrorData", "ServerNotification"];

pub(super) fn build_schema_bundle(schemas: Vec<GeneratedSchema>) -> Result<Value> {
    let namespaced_types = collect_namespaced_types(&schemas);
    let mut definitions = Map::new();

    for schema in schemas {
        let GeneratedSchema {
            namespace,
            logical_name,
            mut value,
            in_v1_dir,
        } = schema;

        if IGNORED_DEFINITIONS.contains(&logical_name.as_str()) {
            continue;
        }

        if let Some(ref ns) = namespace {
            rewrite_refs_to_namespace(&mut value, ns);
        } else {
            rewrite_refs_to_known_namespaces(&mut value, &namespaced_types);
        }

        if let Value::Object(ref mut obj) = value
            && let Some(defs) = obj.remove("definitions")
            && let Value::Object(defs_obj) = defs
        {
            for (def_name, mut def_schema) in defs_obj {
                if IGNORED_DEFINITIONS.contains(&def_name.as_str()) {
                    continue;
                }
                if SPECIAL_DEFINITIONS.contains(&def_name.as_str()) {
                    continue;
                }
                annotate_schema(&mut def_schema, Some(def_name.as_str()));
                let target_namespace = match namespace {
                    Some(ref ns) => Some(ns.clone()),
                    None => namespace_for_definition(&def_name, &namespaced_types)
                        .cloned()
                        .filter(|_| !in_v1_dir),
                };
                if let Some(ref ns) = target_namespace {
                    if namespace.as_deref() == Some(ns.as_str()) {
                        rewrite_refs_to_namespace(&mut def_schema, ns);
                    }
                    // Inferred ownership must not silently discard a distinct
                    // same-named schema. The shared insertion owner checks equivalence.
                    insert_into_namespace(&mut definitions, ns, def_name.clone(), def_schema, false)?;
                } else {
                    insert_definition(&mut definitions, def_name, def_schema, "root", true)?;
                }
            }
        }

        if let Some(ref ns) = namespace {
            insert_into_namespace(&mut definitions, ns, logical_name.clone(), value, true)?;
        } else {
            insert_definition(&mut definitions, logical_name, value, "root", true)?;
        }
    }

    let mut root = Map::new();
    root.insert(
        "$schema".to_string(),
        Value::String("http://json-schema.org/draft-07/schema#".into()),
    );
    root.insert(
        "title".to_string(),
        Value::String("CodexAppServerProtocol".into()),
    );
    root.insert("type".to_string(), Value::String("object".into()));
    root.insert("definitions".to_string(), Value::Object(definitions));

    Ok(Value::Object(root))
}

/// Build a datamodel-code-generator-friendly v2 bundle from the mixed export.
///
/// The full bundle keeps v2 schemas nested under `definitions.v2`, plus a few
/// shared root definitions like `ClientRequest` and `ServerNotification`.
/// Python codegen only walks one definitions map level, so
/// a direct feed would treat `v2` itself as a schema and miss unreferenced v2
/// leaves. This helper flattens all v2 definitions to the root definitions map,
/// then pulls in the shared root schemas and any non-v2 transitive deps they
/// still reference. Keep the shared root unions intact here: some valid
/// request/notification/event variants are inline or only reference shared root
/// helpers, so filtering them by the presence of a `#/definitions/v2/` ref
/// would silently drop real API surface from the flat bundle.
pub(super) fn build_flat_v2_schema(bundle: &Value) -> Result<Value> {
    let Value::Object(root) = bundle else {
        return Err(anyhow!("expected bundle root to be an object"));
    };
    let definitions = root
        .get("definitions")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("expected bundle definitions map"))?;
    let v2_definitions = definitions
        .get("v2")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("expected v2 namespace in bundle definitions"))?;

    let mut flat_root: Map<String, Value> = root
        .iter()
        .filter(|(key, _)| key.as_str() != "definitions")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let title = root
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("CodexAppServerProtocol");
    let mut flat_definitions = v2_definitions.clone();
    let mut shared_definitions = Map::new();
    let mut non_v2_refs = HashSet::new();
    for schema in v2_definitions.values() {
        collect_non_v2_refs_inner(schema, &mut non_v2_refs);
    }
    for schema in flat_definitions.values_mut() {
        rewrite_ref_prefix(schema, "#/definitions/v2/", "#/definitions/");
    }

    for shared in FLAT_V2_SHARED_DEFINITIONS {
        let Some(shared_schema) = definitions.get(*shared) else {
            continue;
        };
        let shared_schema = shared_schema.clone();
        non_v2_refs.extend(collect_non_v2_refs(&shared_schema));
        shared_definitions.insert((*shared).to_string(), shared_schema);
    }

    for name in collect_definition_dependencies(definitions, non_v2_refs) {
        if name == "v2" {
            continue;
        }
        if let Some(schema) = definitions.get(&name) {
            let mut schema = schema.clone();
            rewrite_ref_prefix(&mut schema, "#/definitions/v2/", "#/definitions/");
            insert_definition(&mut flat_definitions, name, schema, "flat v2", false)?;
        }
    }

    for (name, mut schema) in shared_definitions {
        rewrite_ref_prefix(&mut schema, "#/definitions/v2/", "#/definitions/");
        insert_definition(&mut flat_definitions, name, schema, "flat v2", false)?;
    }
    flat_root.insert("title".to_string(), Value::String(format!("{title}V2")));
    flat_root.insert("definitions".to_string(), Value::Object(flat_definitions));
    let mut flat_bundle = Value::Object(flat_root);
    rewrite_ref_prefix(&mut flat_bundle, "#/definitions/v2/", "#/definitions/");
    ensure_no_ref_prefix(&flat_bundle, "#/definitions/v2/", "flat v2")?;
    ensure_referenced_definitions_present(&flat_bundle, "flat v2")?;
    Ok(flat_bundle)
}

fn collect_non_v2_refs(value: &Value) -> HashSet<String> {
    let mut refs = HashSet::new();
    collect_non_v2_refs_inner(value, &mut refs);
    refs
}

fn collect_non_v2_refs_inner(value: &Value, refs: &mut HashSet<String>) {
    if let Some(reference) = value.get("$ref").and_then(Value::as_str)
        && !reference.starts_with("#/definitions/v2/")
        && let Some(name) = super::json_schema::local_definition_name(reference, "definitions")
    {
        refs.insert(name);
    }
    super::json_schema::for_each_schema_child(value, true, &mut |child| collect_non_v2_refs_inner(child, refs));
}

fn collect_definition_dependencies(
    definitions: &Map<String, Value>,
    names: HashSet<String>,
) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut to_process: Vec<String> = names.into_iter().collect();
    while let Some(name) = to_process.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(schema) = definitions.get(&name) else {
            continue;
        };
        for dep in collect_non_v2_refs(schema) {
            if !seen.contains(&dep) {
                to_process.push(dep);
            }
        }
    }
    seen
}

fn rewrite_ref_prefix(value: &mut Value, prefix: &str, replacement: &str) {
    if let Some(reference) = value.get("$ref").and_then(Value::as_str).map(str::to_owned)
        && let Some(suffix) = reference.strip_prefix(prefix)
    {
        value["$ref"] = Value::String(format!("{replacement}{suffix}"));
    }
    super::json_schema::for_each_schema_child_mut(value, true, &mut |child| rewrite_ref_prefix(child, prefix, replacement));
}

fn ensure_no_ref_prefix(value: &Value, prefix: &str, label: &str) -> Result<()> {
    if let Some(reference) = first_ref_with_prefix(value, prefix) {
        return Err(anyhow!(
            "{label} schema still references namespaced definitions; found {reference}"
        ));
    }
    Ok(())
}

fn first_ref_with_prefix(value: &Value, prefix: &str) -> Option<String> {
    if let Some(reference) = value.get("$ref").and_then(Value::as_str)
        && reference.starts_with(prefix)
    { return Some(reference.to_string()); }
    let mut found = None;
    super::json_schema::for_each_schema_child(value, true, &mut |child| {
        if found.is_none() { found = first_ref_with_prefix(child, prefix); }
    });
    found
}

fn ensure_referenced_definitions_present(schema: &Value, label: &str) -> Result<()> {
    let _definitions = schema
        .get("definitions")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("expected definitions map in {label} schema"))?;
    let mut missing = HashSet::new();
    collect_missing_definitions(schema, schema, &mut missing);
    if missing.is_empty() {
        return Ok(());
    }
    let mut missing_names: Vec<String> = missing.into_iter().collect();
    missing_names.sort();
    Err(anyhow!(
        "{label} schema missing definitions: {}",
        missing_names.join(", ")
    ))
}

fn collect_missing_definitions(value: &Value, root: &Value, missing: &mut HashSet<String>) {
    if let Some(reference) = value.get("$ref").and_then(Value::as_str)
        && reference.starts_with("#/definitions/")
        && root.pointer(&reference[1..]).is_none()
    {
        missing.insert(reference.to_string());
    }
    super::json_schema::for_each_schema_child(value, true, &mut |child| collect_missing_definitions(child, root, missing));
}

fn insert_into_namespace(
    definitions: &mut Map<String, Value>,
    namespace: &str,
    name: String,
    schema: Value,
    prefer_standalone: bool,
) -> Result<()> {
    let entry = definitions
        .entry(namespace.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    match entry {
        Value::Object(map) => {
            insert_definition(map, name, schema, &format!("namespace `{namespace}`"), prefer_standalone)
        }
        _ => Err(anyhow!("expected namespace {namespace} to be an object")),
    }
}

fn insert_definition(
    definitions: &mut Map<String, Value>,
    name: String,
    schema: Value,
    location: &str,
    replace_equivalent: bool,
) -> Result<()> {
    if let Some(existing) = definitions.get_mut(&name) {
        if existing == &schema {
            return Ok(());
        }

        // Schemars includes the dialect and type title on standalone roots but
        // omits them on the same type embedded in another schema's definitions.
        // Generated branch titles also depend on standalone/embedded context.
        // Compare validation, preserving dialects and opaque instance data.
        let normalize = |value: &Value| {
            let mut value = value.clone();
            remove_schema_titles(&mut value);
            if let Some(map) = value.as_object_mut() {
                if map.get("$schema").and_then(Value::as_str)
                    == Some("http://json-schema.org/draft-07/schema#")
                {
                    map.remove("$schema");
                }
            }
            value
        };
        if normalize(existing) == normalize(&schema) {
            // Prefer a registered standalone owner's presentation over inferred
            // embedded annotations, without admitting incompatible validation.
            if replace_equivalent {
                *existing = schema;
            }
            return Ok(());
        }

        let existing_title = existing
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("<untitled>");
        let new_title = schema
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("<untitled>");
        return Err(anyhow!(
            "schema definition collision in {location}: {name} (existing title: {existing_title}, new title: {new_title}); use #[schemars(rename = \"...\")] to rename one of the conflicting schema definitions"
        ));
    }

    definitions.insert(name, schema);
    Ok(())
}

/// Recursively rewrite $ref values that point at "#/definitions/..." so that
/// they point to a namespaced location under the bundle.
fn rewrite_refs_to_namespace(value: &mut Value, ns: &str) {
    if let Some(reference) = value.get("$ref").and_then(Value::as_str)
        && let Some(suffix) = reference.strip_prefix("#/definitions/")
        && !suffix.starts_with(&format!("{ns}/"))
    {
        value["$ref"] = Value::String(format!("#/definitions/{ns}/{suffix}"));
    }
    super::json_schema::for_each_schema_child_mut(value, true, &mut |child| rewrite_refs_to_namespace(child, ns));
}

/// Recursively rewrite bare root definition refs to the namespace that owns the
/// referenced type in the bundle.
///
/// The mixed export contains shared root helper schemas that are intentionally
/// left outside the `v2` namespace, but some of their extracted child
/// definitions still contain refs like `#/definitions/ThreadId`. When the real
/// schema only exists under `#/definitions/v2/ThreadId`, those refs become
/// dangling and downstream codegen falls back to placeholder `Any` models. This
/// rewrite keeps the shared helpers at the root while retargeting their refs to
/// the namespaced definitions that actually exist.
fn rewrite_refs_to_known_namespaces(value: &mut Value, types: &HashMap<String, String>) {
    if let Some(reference) = value.get("$ref").and_then(Value::as_str)
        && let Some(suffix) = reference.strip_prefix("#/definitions/")
    {
        let (name, tail) = suffix.split_once('/').map_or((suffix, ""), |(name, tail)| (name, tail));
        if let Some(decoded) = super::json_schema::local_definition_name(reference, "definitions")
            && let Some(ns) = namespace_for_definition(&decoded, types)
        {
            let tail = if tail.is_empty() { String::new() } else { format!("/{tail}") };
            value["$ref"] = Value::String(format!("#/definitions/{ns}/{name}{tail}"));
        }
    }
    super::json_schema::for_each_schema_child_mut(value, true, &mut |child| rewrite_refs_to_known_namespaces(child, types));
}

fn collect_namespaced_types(schemas: &[GeneratedSchema]) -> HashMap<String, String> {
    let mut types = HashMap::new();
    for schema in schemas {
        if let Some(ns) = schema.namespace() {
            types
                .entry(schema.logical_name().to_string())
                .or_insert_with(|| ns.to_string());
            if let Some(Value::Object(defs)) = schema.value().get("definitions") {
                for key in defs.keys() {
                    types.entry(key.clone()).or_insert_with(|| ns.to_string());
                }
            }
            if let Some(Value::Object(defs)) = schema.value().get("$defs") {
                for key in defs.keys() {
                    types.entry(key.clone()).or_insert_with(|| ns.to_string());
                }
            }
        }
    }
    types
}

fn namespace_for_definition<'a>(
    name: &str,
    types: &'a HashMap<String, String>,
) -> Option<&'a String> {
    if let Some(ns) = types.get(name) {
        return Some(ns);
    }
    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed != name {
        return types.get(trimmed);
    }
    None
}

