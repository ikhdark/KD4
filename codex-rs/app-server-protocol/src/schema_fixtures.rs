use crate::ClientNotification;
use crate::ClientRequest;
use crate::OverloadErrorData;
use crate::PluginRemoteErrorData;
use crate::ServerNotification;
use crate::ServerRequest;
use crate::ThreadErrorData;
use crate::export::GENERATED_TS_HEADER;
use crate::export::filter_experimental_ts_tree;
use crate::export::generate_index_ts_tree;
use crate::export::trim_trailing_line_whitespace;
use crate::protocol::common::visit_client_response_types;
use crate::protocol::common::visit_server_response_types;
use anyhow::Context;
use anyhow::Result;
use serde_json::Map;
use serde_json::Value;
use std::any::TypeId;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use ts_rs::TS;
use ts_rs::TypeVisitor;

#[derive(Clone, Copy, Debug, Default)]
pub struct SchemaFixtureOptions {
    pub experimental_api: bool,
}

pub fn read_schema_fixture_subtree(
    schema_root: &Path,
    label: &str,
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let subtree_root = schema_root.join(label);
    collect_files_recursive(&subtree_root)
        .with_context(|| format!("read schema fixture subtree {}", subtree_root.display()))
}

#[doc(hidden)]
pub fn generate_typescript_schema_fixture_subtree_for_tests() -> Result<BTreeMap<PathBuf, Vec<u8>>>
{
    let mut files = BTreeMap::new();
    let mut seen = HashSet::new();

    collect_typescript_fixture_file::<ClientRequest>(&mut files, &mut seen)?;
    visit_typescript_fixture_dependencies(&mut files, &mut seen, |visitor| {
        visit_client_response_types(visitor);
    })?;
    collect_typescript_fixture_file::<ClientNotification>(&mut files, &mut seen)?;
    collect_typescript_fixture_file::<ServerRequest>(&mut files, &mut seen)?;
    visit_typescript_fixture_dependencies(&mut files, &mut seen, |visitor| {
        visit_server_response_types(visitor);
    })?;
    collect_typescript_fixture_file::<ServerNotification>(&mut files, &mut seen)?;
    collect_typescript_fixture_file::<OverloadErrorData>(&mut files, &mut seen)?;
    collect_typescript_fixture_file::<PluginRemoteErrorData>(&mut files, &mut seen)?;
    collect_typescript_fixture_file::<ThreadErrorData>(&mut files, &mut seen)?;

    filter_experimental_ts_tree(&mut files)?;
    generate_index_ts_tree(&mut files);
    for content in files.values_mut() {
        *content = trim_trailing_line_whitespace(content);
    }

    Ok(files
        .into_iter()
        .map(|(path, content)| (path, content.into_bytes()))
        .collect())
}

/// Regenerates schema fixtures with configurable options.
pub fn write_schema_fixtures_with_options(
    schema_root: &Path,
    prettier: Option<&Path>,
    options: SchemaFixtureOptions,
) -> Result<()> {
    std::fs::create_dir_all(schema_root)?;
    let staging = tempfile::Builder::new()
        .prefix(".schema-staging-")
        .tempdir_in(schema_root)?;
    let typescript_out_dir = staging.path().join("typescript");
    let json_out_dir = staging.path().join("json");

    crate::generate_ts_with_options(
        &typescript_out_dir,
        prettier,
        crate::GenerateTsOptions {
            experimental_api: options.experimental_api,
            ..crate::GenerateTsOptions::default()
        },
    )?;
    crate::generate_json_with_experimental(&json_out_dir, options.experimental_api)?;

    // Publish only after both generators (including the formatter) succeed.
    // Replacement of the two owned subtrees is not a two-directory transaction.
    for label in ["typescript", "json"] {
        let destination = schema_root.join(label);
        if destination.exists() {
            std::fs::remove_dir_all(&destination)
                .with_context(|| format!("failed to remove {}", destination.display()))?;
        }
        std::fs::rename(staging.path().join(label), &destination)
            .with_context(|| format!("failed to publish {}", destination.display()))?;
    }
    Ok(())
}

fn read_file_bytes(path: &Path) -> Result<Vec<u8>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    if path.extension().is_some_and(|ext| ext == "json") {
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse JSON in {}", path.display()))?;
        let value = canonicalize_json(&value);
        let normalized = serde_json::to_vec_pretty(&value)
            .with_context(|| format!("failed to reserialize JSON in {}", path.display()))?;
        return Ok(normalized);
    }
    if path.extension().is_some_and(|ext| ext == "ts") {
        // Windows checkouts (and some generators) may produce CRLF; normalize so the
        // fixture test is platform-independent.
        let text = String::from_utf8(bytes)
            .with_context(|| format!("expected UTF-8 TypeScript in {}", path.display()))?;
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        // Fixture comparisons care about schema content, not whether the generator
        // re-prepended the standard banner to every TypeScript file.
        let text = text
            .strip_prefix(GENERATED_TS_HEADER)
            .unwrap_or(&text)
            .to_string();
        return Ok(text.into_bytes());
    }
    Ok(bytes)
}

// Only schema keywords define unordered arrays. Literal data and tuple positions
// preserve their order even when their contents resemble schema keywords.
fn canonicalize_json(value: &Value) -> Value {
    let Value::Object(map) = value else {
        return canonicalize_literal(value);
    };
    let mut sorted = Map::new();
    for (key, child) in map {
        let normalized = match key.as_str() {
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" => {
                if let Value::Object(entries) = child {
                    Value::Object(
                        entries
                            .iter()
                            .map(|(name, schema)| {
                                // Mixed app-server bundles group definitions under v1/v2.
                                let schema = if key == "definitions"
                                    && matches!(name.as_str(), "v1" | "v2")
                                {
                                    match schema.as_object() {
                                        Some(namespace) => Value::Object(
                                            namespace
                                                .iter()
                                                .map(|(name, schema)| {
                                                    (name.clone(), canonicalize_json(schema))
                                                })
                                                .collect(),
                                        ),
                                        None => canonicalize_literal(schema),
                                    }
                                } else {
                                    canonicalize_json(schema)
                                };
                                (name.clone(), schema)
                            })
                            .collect(),
                    )
                } else {
                    canonicalize_literal(child)
                }
            }
            "anyOf" | "oneOf" | "allOf" | "items" | "prefixItems" => {
                if let Value::Array(items) = child {
                    let mut items: Vec<_> = items.iter().map(canonicalize_json).collect();
                    if matches!(key.as_str(), "anyOf" | "oneOf" | "allOf") {
                        items.sort_by_cached_key(Value::to_string);
                    }
                    Value::Array(items)
                } else {
                    canonicalize_json(child)
                }
            }
            "required" | "type" | "enum" => {
                let mut child = canonicalize_literal(child);
                if let Value::Array(items) = &mut child {
                    items.sort_by_cached_key(Value::to_string);
                }
                child
            }
            "additionalProperties"
            | "additionalItems"
            | "contains"
            | "not"
            | "if"
            | "then"
            | "else"
            | "propertyNames"
            | "unevaluatedProperties"
            | "unevaluatedItems" => canonicalize_json(child),
            _ => canonicalize_literal(child),
        };
        sorted.insert(key.clone(), normalized);
    }
    Value::Object(sorted)
}

fn canonicalize_literal(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, child)| (key.clone(), canonicalize_literal(child)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_literal).collect()),
        _ => value.clone(),
    }
}

fn collect_files_recursive(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("failed to read dir {}", dir.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to read dir entry in {}", dir.display()))?;
            let path = entry.path();
            // Resource layouts may contain symlinks. `DirEntry::file_type()` does not follow
            // them, so use `metadata()` to treat symlinks as the files or directories they
            // reference.
            let metadata = std::fs::metadata(&path)
                .with_context(|| format!("failed to stat {}", path.display()))?;
            if metadata.is_dir() {
                stack.push(path);
                continue;
            } else if !metadata.is_file() {
                continue;
            }

            let rel = path
                .strip_prefix(root)
                .with_context(|| {
                    format!(
                        "failed to strip prefix {} from {}",
                        root.display(),
                        path.display()
                    )
                })?
                .to_path_buf();

            files.insert(rel, read_file_bytes(&path)?);
        }
    }

    Ok(files)
}

fn collect_typescript_fixture_file<T: TS + 'static + ?Sized>(
    files: &mut BTreeMap<PathBuf, String>,
    seen: &mut HashSet<TypeId>,
) -> Result<()> {
    let Some(output_path) = T::output_path() else {
        return Ok(());
    };
    if !seen.insert(TypeId::of::<T>()) {
        return Ok(());
    }

    let contents = T::export_to_string().context("export TypeScript fixture content")?;
    let output_path = normalize_relative_fixture_path(&output_path);
    let contents = contents.replace("\r\n", "\n").replace('\r', "\n");
    if let Some(existing) = files.get(&output_path) {
        anyhow::ensure!(
            existing == &contents,
            "conflicting TypeScript fixture {} for {}",
            output_path.display(),
            std::any::type_name::<T>()
        );
    } else {
        files.insert(output_path, contents);
    }

    let mut visitor = TypeScriptFixtureCollector {
        files,
        seen,
        error: None,
    };
    T::visit_dependencies(&mut visitor);
    if let Some(error) = visitor.error {
        return Err(error);
    }

    Ok(())
}

fn normalize_relative_fixture_path(path: &Path) -> PathBuf {
    path.components().collect()
}

fn visit_typescript_fixture_dependencies(
    files: &mut BTreeMap<PathBuf, String>,
    seen: &mut HashSet<TypeId>,
    visit: impl FnOnce(&mut TypeScriptFixtureCollector<'_>),
) -> Result<()> {
    let mut visitor = TypeScriptFixtureCollector {
        files,
        seen,
        error: None,
    };
    visit(&mut visitor);
    if let Some(error) = visitor.error {
        return Err(error);
    }
    Ok(())
}

struct TypeScriptFixtureCollector<'a> {
    files: &'a mut BTreeMap<PathBuf, String>,
    seen: &'a mut HashSet<TypeId>,
    error: Option<anyhow::Error>,
}

impl TypeVisitor for TypeScriptFixtureCollector<'_> {
    fn visit<T: TS + 'static + ?Sized>(&mut self) {
        if self.error.is_some() {
            return;
        }
        self.error = collect_typescript_fixture_file::<T>(self.files, self.seen).err();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn read_normalized(value: Value) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("json")).unwrap();
        std::fs::write(dir.path().join("json/schema.json"), value.to_string()).unwrap();
        read_schema_fixture_subtree(dir.path(), "json")
            .unwrap()
            .remove(Path::new("schema.json"))
            .unwrap()
    }

    #[test]
    fn fixture_reader_preserves_ordered_data_and_tuple_positions() {
        for (left, right) in [
            (
                serde_json::json!({"const":[1,2]}),
                serde_json::json!({"const":[2,1]}),
            ),
            (
                serde_json::json!({"enum":[[1,2]]}),
                serde_json::json!({"enum":[[2,1]]}),
            ),
            (
                serde_json::json!({"default":{"required":["a","b"]}}),
                serde_json::json!({"default":{"required":["b","a"]}}),
            ),
            (
                serde_json::json!({"const":{"anyOf":[{"title":"A"},{"title":"B"}]}}),
                serde_json::json!({"const":{"anyOf":[{"title":"B"},{"title":"A"}]}}),
            ),
            (
                serde_json::json!({"items":[{"$ref":"#/A"},{"$ref":"#/B"}]}),
                serde_json::json!({"items":[{"$ref":"#/B"},{"$ref":"#/A"}]}),
            ),
            (
                serde_json::json!({"prefixItems":[{"title":"A"},{"title":"B"}]}),
                serde_json::json!({"prefixItems":[{"title":"B"},{"title":"A"}]}),
            ),
        ] {
            assert_ne!(read_normalized(left), read_normalized(right));
        }
    }

    #[test]
    fn fixture_reader_normalizes_unordered_schema_keywords() {
        assert_eq!(
            read_normalized(
                serde_json::json!({"properties":{"required":{"required":["b","a"],"anyOf":[{"$ref":"#/B"},{"$ref":"#/A"}]}}})
            ),
            read_normalized(
                serde_json::json!({"properties":{"required":{"required":["a","b"],"anyOf":[{"$ref":"#/A"},{"$ref":"#/B"}]}}})
            ),
        );
    }

    #[test]
    fn fixture_generation_failure_preserves_existing_outputs() -> Result<()> {
        let dir = tempfile::tempdir()?;
        for label in ["typescript", "json"] {
            std::fs::create_dir(dir.path().join(label))?;
            std::fs::write(dir.path().join(label).join("previous"), "keep me")?;
        }
        let result = write_schema_fixtures_with_options(
            dir.path(),
            Some(&dir.path().join("missing-prettier")),
            SchemaFixtureOptions::default(),
        );
        assert!(result.is_err());
        for label in ["typescript", "json"] {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(label).join("previous"))?,
                "keep me"
            );
        }
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 2);
        Ok(())
    }

    #[test]
    fn typescript_collector_rejects_conflicting_output_paths() -> Result<()> {
        #[derive(TS)]
        #[ts(rename = "Collision", export_to = "Collision.ts")]
        #[expect(dead_code, reason = "Only the derived TypeScript shape is exercised")]
        struct First {
            value: String,
        }
        #[derive(TS)]
        #[ts(rename = "Collision", export_to = "Collision.ts")]
        #[expect(dead_code, reason = "Only the derived TypeScript shape is exercised")]
        struct Second {
            value: bool,
        }
        #[derive(TS)]
        #[ts(rename = "Collision", export_to = "Collision.ts")]
        #[expect(dead_code, reason = "Only the derived TypeScript shape is exercised")]
        struct Identical {
            value: String,
        }
        let mut files = BTreeMap::new();
        let mut seen = HashSet::new();
        collect_typescript_fixture_file::<First>(&mut files, &mut seen)?;
        collect_typescript_fixture_file::<Identical>(&mut files, &mut seen)?;
        let error = collect_typescript_fixture_file::<Second>(&mut files, &mut seen).unwrap_err();
        assert!(error.to_string().contains("Collision.ts"));
        assert!(files[Path::new("Collision.ts")].contains("value: string"));
        Ok(())
    }
}
