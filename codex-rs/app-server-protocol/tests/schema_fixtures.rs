use anyhow::Context;
use anyhow::Result;
use codex_app_server_protocol::generate_json_with_experimental;
use codex_app_server_protocol::generate_typescript_schema_fixture_subtree_for_tests;
use codex_app_server_protocol::read_schema_fixture_subtree;
use similar::TextDiff;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

#[test]
fn legacy_generator_convenience_apis_are_not_exposed() {
    let lib_source = include_str!("../src/lib.rs");
    let export_source = include_str!("../src/export.rs");
    let fixture_source = include_str!("../src/schema_fixtures.rs");
    let common_source = include_str!("../src/protocol/common.rs");

    for removed_export in [
        "pub use export::generate_json;",
        "pub use export::generate_ts;",
        "pub use export::generate_types;",
        "pub use schema_fixtures::read_schema_fixture_tree;",
        "pub use schema_fixtures::write_schema_fixtures;",
    ] {
        assert!(
            !lib_source.contains(removed_export),
            "legacy convenience API is still exported: {removed_export}"
        );
    }

    for removed_declaration in [
        "pub fn generate_json(",
        "pub fn generate_ts(",
        "pub fn generate_types(",
        "pub struct GeneratedSchema",
    ] {
        assert!(
            !export_source.contains(removed_declaration),
            "internal generator API is still exposed: {removed_declaration}"
        );
    }

    for removed_declaration in [
        "pub fn read_schema_fixture_tree(",
        "pub fn write_schema_fixtures(",
    ] {
        assert!(
            !fixture_source.contains(removed_declaration),
            "unused fixture API is still exposed: {removed_declaration}"
        );
    }

    for crate_internal_exporter in [
        "export_client_response_schemas",
        "export_client_param_schemas",
        "export_server_response_schemas",
        "export_server_param_schemas",
        "export_server_notification_schemas",
        "export_client_notification_schemas",
    ] {
        assert!(
            !common_source.contains(&format!("pub fn {crate_internal_exporter}(")),
            "crate-internal schema exporter is still public: {crate_internal_exporter}"
        );
    }
}

#[test]
fn typescript_schema_fixtures_match_generated() -> Result<()> {
    let schema_root = schema_root()?;
    let fixture_tree = read_tree(&schema_root, "typescript")?;
    let generated_tree = generate_typescript_schema_fixture_subtree_for_tests()
        .context("generate in-memory typescript schema fixtures")?;

    assert_schema_trees_match("typescript", &fixture_tree, &generated_tree)?;

    Ok(())
}

#[test]
fn typescript_schema_relative_modules_resolve() -> Result<()> {
    let schema_root = schema_root()?;
    let typescript_root = schema_root.join("typescript");
    let fixture_tree = read_tree(&schema_root, "typescript")?;

    for (relative_path, contents) in fixture_tree {
        let source = std::str::from_utf8(&contents)
            .with_context(|| format!("decode {} as UTF-8", relative_path.display()))?;
        for line in source.lines() {
            let Some((_, import_suffix)) = line.split_once(" from \"") else {
                continue;
            };
            let Some((module, _)) = import_suffix.split_once('"') else {
                continue;
            };
            if !module.starts_with('.') {
                continue;
            }

            let source_path = typescript_root.join(&relative_path);
            let target_file = source_path
                .parent()
                .context("generated TypeScript source has no parent")?
                .join(format!("{module}.ts"));
            let target_index = source_path
                .parent()
                .context("generated TypeScript source has no parent")?
                .join(module)
                .join("index.ts");
            assert!(
                target_file.is_file() || target_index.is_file(),
                "generated TypeScript module {} imports missing relative module {module}",
                relative_path.display()
            );
        }
    }

    Ok(())
}

#[test]
fn json_schema_fixtures_match_generated() -> Result<()> {
    assert_schema_fixtures_match_generated("json", |output_dir| {
        generate_json_with_experimental(output_dir, /*experimental_api*/ false)
    })
}

#[test]
fn typed_jsonrpc_error_payloads_are_generator_roots() -> Result<()> {
    let typescript = generate_typescript_schema_fixture_subtree_for_tests()
        .context("generate in-memory typescript schema fixtures")?;
    for path in [
        "OverloadErrorData.ts",
        "OverloadReason.ts",
        "v2/PluginRemoteErrorData.ts",
        "v2/PluginRemoteErrorReason.ts",
        "v2/ThreadErrorData.ts",
        "v2/ThreadErrorReason.ts",
    ] {
        assert!(
            typescript.contains_key(Path::new(path)),
            "missing generated TypeScript error contract {path}"
        );
    }

    let temp_dir = tempfile::tempdir().context("create temp dir")?;
    generate_json_with_experimental(temp_dir.path(), /*experimental_api*/ false)
        .context("generate JSON schema fixtures")?;
    for path in [
        "OverloadErrorData.json",
        "v2/PluginRemoteErrorData.json",
        "v2/ThreadErrorData.json",
    ] {
        assert!(
            temp_dir.path().join(path).is_file(),
            "missing generated JSON error contract {path}"
        );
    }

    let flat_bundle: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            temp_dir
                .path()
                .join("codex_app_server_protocol.v2.schemas.json"),
        )
        .context("read flat v2 schema bundle")?,
    )
    .context("parse flat v2 schema bundle")?;
    let definitions = flat_bundle
        .get("definitions")
        .and_then(serde_json::Value::as_object)
        .context("flat v2 schema definitions")?;
    for name in [
        "OverloadErrorData",
        "OverloadReason",
        "PluginRemoteErrorData",
        "PluginRemoteErrorReason",
        "ThreadErrorData",
        "ThreadErrorReason",
    ] {
        assert!(
            definitions.contains_key(name),
            "flat v2 schema bundle is missing {name}"
        );
    }

    Ok(())
}

fn assert_schema_fixtures_match_generated(
    label: &'static str,
    generate: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let schema_root = schema_root()?;
    let fixture_tree = read_tree(&schema_root, label)?;

    let temp_dir = tempfile::tempdir().context("create temp dir")?;
    let generated_root = temp_dir.path().join(label);
    generate(&generated_root).with_context(|| {
        format!(
            "generate {label} schema fixtures into {}",
            generated_root.display()
        )
    })?;

    let generated_tree = read_tree(temp_dir.path(), label)?;

    assert_schema_trees_match(label, &fixture_tree, &generated_tree)?;

    Ok(())
}

fn assert_schema_trees_match(
    label: &str,
    fixture_tree: &BTreeMap<PathBuf, Vec<u8>>,
    generated_tree: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<()> {
    let fixture_paths = fixture_tree
        .keys()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>();
    let generated_paths = generated_tree
        .keys()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>();

    if fixture_paths != generated_paths {
        let expected = fixture_paths.join("\n");
        let actual = generated_paths.join("\n");
        let diff = TextDiff::from_lines(&expected, &actual)
            .unified_diff()
            .header("fixture", "generated")
            .to_string();

        panic!(
            "Vendored {label} app-server schema fixture file set doesn't match freshly generated output. \
Run `just app-server-schema-regenerate <owner>` to overwrite with your changes.\n\n{diff}"
        );
    }

    // If the file sets match, diff contents for each file for a nicer error.
    for (path, expected) in fixture_tree {
        let actual = generated_tree
            .get(path)
            .ok_or_else(|| anyhow::anyhow!("missing generated file: {}", path.display()))?;

        if expected == actual {
            continue;
        }

        let expected_str = String::from_utf8_lossy(expected);
        let actual_str = String::from_utf8_lossy(actual);
        let diff = TextDiff::from_lines(&expected_str, &actual_str)
            .unified_diff()
            .header("fixture", "generated")
            .to_string();
        panic!(
            "Vendored {label} app-server schema fixture {} differs from generated output. \
Run `just app-server-schema-regenerate <owner>` to overwrite with your changes.\n\n{diff}",
            path.display()
        );
    }

    Ok(())
}

fn schema_root() -> Result<PathBuf> {
    // Resolve a known file, then walk up to the schema root so the fixture path
    // remains anchored to the consuming crate.
    let typescript_index = codex_utils_cargo_bin::find_resource!("schema/typescript/index.ts")
        .context("resolve TypeScript schema index.ts")?;
    let schema_root = typescript_index
        .parent()
        .and_then(|p| p.parent())
        .context("derive schema root from schema/typescript/index.ts")?
        .to_path_buf();

    // Sanity check that the JSON fixtures resolve to the same schema root.
    let json_bundle =
        codex_utils_cargo_bin::find_resource!("schema/json/codex_app_server_protocol.schemas.json")
            .context("resolve JSON schema bundle")?;
    let json_root = json_bundle
        .parent()
        .and_then(|p| p.parent())
        .context("derive schema root from schema/json/codex_app_server_protocol.schemas.json")?;
    anyhow::ensure!(
        schema_root == json_root,
        "schema roots disagree: typescript={} json={}",
        schema_root.display(),
        json_root.display()
    );

    Ok(schema_root)
}

fn read_tree(root: &Path, label: &str) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    read_schema_fixture_subtree(root, label).with_context(|| {
        format!(
            "read {label} schema fixture subtree from {}",
            root.display()
        )
    })
}
