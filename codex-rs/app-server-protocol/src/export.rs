use crate::ClientNotification;
use crate::ClientRequest;
use crate::ServerNotification;
use crate::ServerRequest;
use crate::experimental_api::experimental_fields;
use crate::export_client_notification_schemas;
use crate::export_client_param_schemas;
use crate::export_client_response_schemas;
use crate::export_client_responses;
use crate::export_server_notification_schemas;
use crate::export_server_param_schemas;
use crate::export_server_response_schemas;
use crate::export_server_responses;
use crate::protocol::common::EXPERIMENTAL_CLIENT_METHOD_PARAM_TYPES;
use crate::protocol::common::EXPERIMENTAL_CLIENT_METHOD_RESPONSE_TYPES;
use crate::protocol::common::EXPERIMENTAL_CLIENT_METHODS;
use crate::protocol::common::EXPERIMENTAL_SERVER_METHOD_PARAM_TYPES;
use crate::protocol::common::EXPERIMENTAL_SERVER_METHOD_RESPONSE_TYPES;
use crate::protocol::common::EXPERIMENTAL_SERVER_METHODS;
use crate::protocol::common::SERVER_NOTIFICATION_METADATA;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use codex_protocol::protocol::RolloutLine;
use schemars::JsonSchema;
use schemars::schema_for;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use ts_rs::TS;

pub(crate) const GENERATED_TS_HEADER: &str = "// GENERATED CODE! DO NOT MODIFY BY HAND!\n\n";
// Header processing also visits existing user files in the output directory.
// Support up to 64 MiB per TypeScript file without an unbounded allocation.
const MAX_TYPESCRIPT_HEADER_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const IGNORED_DEFINITIONS: &[&str] = &["Option<()>"];
const JSON_V1_ALLOWLIST: &[&str] = &["InitializeParams", "InitializeResponse"];
const EXPERIMENTAL_CLIENT_METHOD_DEPENDENCY_TYPES: &[&str] = &[
    "EnvironmentShellInfo",
    "RemoteControlClient",
    "RemoteControlClientsListOrder",
    "ThreadBackgroundTerminal",
];
const SPECIAL_DEFINITIONS: &[&str] = &[
    "ClientNotification",
    "ClientRequest",
    "ServerNotification",
    "ServerRequest",
];
const FLAT_V2_SHARED_DEFINITIONS: &[&str] =
    &["ClientRequest", "OverloadErrorData", "ServerNotification"];
const V1_CLIENT_REQUEST_METHODS: &[&str] =
    &["getConversationSummary", "gitDiffToRemote", "getAuthStatus"];

#[derive(Clone)]
pub(crate) struct GeneratedSchema {
    namespace: Option<String>,
    logical_name: String,
    value: Value,
    in_v1_dir: bool,
}

impl GeneratedSchema {
    fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    fn logical_name(&self) -> &str {
        &self.logical_name
    }

    fn value(&self) -> &Value {
        &self.value
    }
}

type JsonSchemaEmitter = fn() -> Result<GeneratedSchema>;

#[derive(Clone, Copy, Debug)]
pub struct GenerateTsOptions {
    pub generate_indices: bool,
    pub ensure_headers: bool,
    pub run_prettier: bool,
    pub experimental_api: bool,
}

impl Default for GenerateTsOptions {
    fn default() -> Self {
        Self {
            generate_indices: true,
            ensure_headers: true,
            run_prettier: true,
            experimental_api: false,
        }
    }
}

pub fn generate_ts_with_options(
    out_dir: &Path,
    prettier: Option<&Path>,
    options: GenerateTsOptions,
) -> Result<()> {
    let v2_out_dir = out_dir.join("v2");
    ensure_dir(out_dir)?;
    ensure_dir(&v2_out_dir)?;

    ClientRequest::export_all_to(out_dir)?;
    export_client_responses(out_dir)?;
    ClientNotification::export_all_to(out_dir)?;

    ServerRequest::export_all_to(out_dir)?;
    export_server_responses(out_dir)?;
    ServerNotification::export_all_to(out_dir)?;

    crate::OverloadErrorData::export_all_to(out_dir)?;
    crate::PluginRemoteErrorData::export_all_to(out_dir)?;
    crate::ThreadErrorData::export_all_to(out_dir)?;

    if !options.experimental_api {
        filter_experimental_ts(out_dir)?;
    }

    if options.generate_indices {
        generate_index_ts(out_dir)?;
        generate_index_ts(&v2_out_dir)?;
    }

    // Ensure our header is present on all TS files (root + subdirs like v2/).
    let ts_files = ts_files_in_recursive(out_dir)?;

    if options.ensure_headers {
        let worker_count = thread::available_parallelism()
            .map_or(1, usize::from)
            .min(ts_files.len().max(1));
        let chunk_size = ts_files.len().div_ceil(worker_count);
        thread::scope(|scope| -> Result<()> {
            let mut workers = Vec::new();
            for chunk in ts_files.chunks(chunk_size.max(1)) {
                workers.push(scope.spawn(move || -> Result<()> {
                    for file in chunk {
                        prepend_header_if_missing(file)?;
                    }
                    Ok(())
                }));
            }

            for worker in workers {
                worker.join().map_err(|payload| {
                    let message = payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| payload.downcast_ref::<&str>().copied())
                        .unwrap_or("non-string panic payload");
                    anyhow!("TypeScript header worker panicked: {message}")
                })??;
            }

            Ok(())
        })?;
    }

    // Optionally run Prettier on all generated TS files.
    if options.run_prettier
        && let Some(prettier_bin) = prettier
        && !ts_files.is_empty()
    {
        let prettier_files = match std::env::current_dir() {
            Ok(current_dir) => {
                std::borrow::Cow::Owned(prettier_file_arguments(&ts_files, &current_dir))
            }
            Err(_) => std::borrow::Cow::Borrowed(ts_files.as_slice()),
        };
        for batch in prettier_file_batches(prettier_bin, &prettier_files)? {
            let status = Command::new(prettier_bin)
                .arg("--write")
                .arg("--log-level")
                .arg("warn")
                .args(batch)
                .status()
                .with_context(|| {
                    format!("Failed to invoke Prettier at {}", prettier_bin.display())
                })?;
            if !status.success() {
                return Err(anyhow!("Prettier failed with status {status}"));
            }
        }
    }

    trim_trailing_whitespace_in_ts_files(&ts_files)?;

    Ok(())
}

fn prettier_file_arguments(files: &[PathBuf], current_dir: &Path) -> Vec<PathBuf> {
    files
        .iter()
        .map(|file| {
            // Keep the process cwd unchanged: executable, config, and ignore-file
            // resolution must not change merely to shorten repeated prefixes.
            let Ok(relative) = file.strip_prefix(current_dir) else {
                return file.clone();
            };
            let relative = Path::new(".").join(relative);
            if relative.as_os_str().as_encoded_bytes().len()
                < file.as_os_str().as_encoded_bytes().len()
            {
                relative
            } else {
                file.clone()
            }
        })
        .collect()
}

// Conservative allowance for Windows quoting/escaping, below the process limit.
fn prettier_file_batches<'a>(prettier: &Path, files: &'a [PathBuf]) -> Result<Vec<&'a [PathBuf]>> {
    const LIMIT: usize = 30_000;
    let argument_size = |path: &Path| path.as_os_str().as_encoded_bytes().len() * 2 + 3;
    let base = argument_size(prettier) + 64;
    let mut batches = Vec::new();
    let mut start = 0;
    let mut size = base;
    for (index, file) in files.iter().enumerate() {
        let next = argument_size(file);
        anyhow::ensure!(
            base + next <= LIMIT,
            "Prettier file argument is too long: {}",
            file.display()
        );
        if size + next > LIMIT {
            batches.push(&files[start..index]);
            start = index;
            size = base;
        }
        size += next;
    }
    if start < files.len() {
        batches.push(&files[start..]);
    }
    Ok(batches)
}

pub fn generate_internal_json_schema(out_dir: &Path) -> Result<()> {
    ensure_dir(out_dir)?;
    write_json_schema::<RolloutLine>(out_dir, "RolloutLine")?;
    Ok(())
}

pub fn generate_json_with_experimental(out_dir: &Path, experimental_api: bool) -> Result<()> {
    ensure_dir(out_dir)?;
    let mut generated_paths = HashSet::new();
    for (relative_path, bytes) in generate_json_schema_tree(experimental_api)? {
        let path = out_dir.join(&relative_path);
        if let Some(parent) = path.parent() {
            ensure_dir(parent)?;
        }
        fs::write(&path, bytes).with_context(|| format!("Failed to write {}", path.display()))?;
        if !experimental_api {
            generated_paths.insert(relative_path);
        }
    }
    if !experimental_api {
        // Freshly generated values were already filtered. Preserve cleanup only
        // for other files that remain in a reused output tree.
        filter_experimental_json_files(
            out_dir,
            &generated_paths,
            &experimental_fields(),
            &experimental_methods(),
            &experimental_method_types(),
        )?;
    }
    Ok(())
}

/// Generate exactly the JSON fixture bytes without materializing a scratch tree.
#[doc(hidden)]
pub fn generate_json_schema_tree(experimental_api: bool) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    generate_json_schema_values(experimental_api)?
        .into_iter()
        .map(|(path, value)| Ok((path, pretty_json_value_bytes(value)?)))
        .collect()
}

pub(crate) fn generate_json_schema_values(experimental_api: bool) -> Result<BTreeMap<PathBuf, Value>> {
    let envelope_emitters: Vec<JsonSchemaEmitter> = vec![
        || generate_json_schema::<crate::RequestId>("RequestId"),
        || generate_json_schema::<crate::JSONRPCMessage>("JSONRPCMessage"),
        || generate_json_schema::<crate::JSONRPCRequest>("JSONRPCRequest"),
        || generate_json_schema::<crate::JSONRPCNotification>("JSONRPCNotification"),
        || generate_json_schema::<crate::JSONRPCResponse>("JSONRPCResponse"),
        || generate_json_schema::<crate::JSONRPCError>("JSONRPCError"),
        || generate_json_schema::<crate::JSONRPCErrorError>("JSONRPCErrorError"),
        || generate_json_schema::<crate::OverloadErrorData>("OverloadErrorData"),
        || generate_json_schema::<crate::PluginRemoteErrorData>("v2::PluginRemoteErrorData"),
        || generate_json_schema::<crate::ThreadErrorData>("v2::ThreadErrorData"),
        || generate_json_schema::<crate::ClientRequest>("ClientRequest"),
        || generate_json_schema::<crate::ServerRequest>("ServerRequest"),
        || generate_json_schema::<crate::ClientNotification>("ClientNotification"),
        || generate_json_schema::<crate::ServerNotification>("ServerNotification"),
    ];

    let mut schemas: Vec<GeneratedSchema> = Vec::new();
    for emit in &envelope_emitters {
        schemas.push(emit()?);
    }

    schemas.extend(export_client_param_schemas()?);
    schemas.extend(export_client_response_schemas()?);
    schemas.extend(export_server_param_schemas()?);
    schemas.extend(export_server_response_schemas()?);
    schemas.extend(export_client_notification_schemas()?);
    schemas.extend(export_server_notification_schemas()?);
    schemas
        .retain(|schema| !schema.in_v1_dir || JSON_V1_ALLOWLIST.contains(&schema.logical_name()));

    let mut tree = schemas
        .iter()
        .filter_map(|schema| {
            json_schema_relative_path(schema).map(|path| (path, schema.value.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut bundle = build_schema_bundle(schemas)?;
    let registered_fields = experimental_fields();
    let method_types = experimental_method_types();
    let methods = experimental_methods();
    if !experimental_api {
        filter_experimental_schema_with_metadata(
            &mut bundle,
            &registered_fields,
            &methods,
            &method_types,
        );
        for value in tree.values_mut() {
            filter_experimental_schema_with_metadata(
                value,
                &registered_fields,
                &methods,
                &method_types,
            );
        }
        for type_name in &method_types {
            for directory in ["", "v1", "v2"] {
                tree.remove(&Path::new(directory).join(format!("{type_name}.json")));
            }
        }
    }
    let flat_v2_bundle = build_flat_v2_schema(&bundle)?;
    tree.insert(
        PathBuf::from("codex_app_server_protocol.schemas.json"),
        bundle,
    );
    tree.insert(
        PathBuf::from("codex_app_server_protocol.v2.schemas.json"),
        flat_v2_bundle,
    );
    Ok(tree)
}

fn filter_experimental_ts(out_dir: &Path) -> Result<()> {
    let registered_fields = experimental_fields();
    let experimental_method_types = experimental_method_types();
    // Most generated TS files are filtered by schema processing, but
    // Request unions and types with `#[experimental(...)]` fields need direct
    // post-processing because they encode method/field information locally.
    filter_request_ts(out_dir, "ClientRequest.ts", EXPERIMENTAL_CLIENT_METHODS)?;
    filter_request_ts(out_dir, "ServerRequest.ts", EXPERIMENTAL_SERVER_METHODS)?;
    filter_experimental_type_fields_ts(out_dir, &registered_fields)?;
    remove_generated_type_files(out_dir, &experimental_method_types, "ts")?;
    Ok(())
}


/// Removes union arms from a generated request type for methods marked experimental.
fn filter_request_ts(out_dir: &Path, file_name: &str, experimental_methods: &[&str]) -> Result<()> {
    let path = out_dir.join(file_name);
    if !path.exists() {
        return Ok(());
    }
    let mut content =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    content = filter_request_ts_contents(content, experimental_methods)?;

    fs::write(&path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

fn filter_request_ts_contents(
    mut content: String,
    experimental_methods: &[&str],
) -> Result<String> {
    let (prefix, body, suffix) =
        split_type_alias(&content).context("cannot parse generated request type alias")?;
    let experimental_methods: HashSet<&str> = experimental_methods
        .iter()
        .copied()
        .filter(|method| !method.is_empty())
        .collect();
    let arms = split_top_level(&body, '|');
    let filtered_arms: Vec<String> = arms
        .into_iter()
        .filter(|arm| {
            extract_method_from_arm(arm)
                .is_none_or(|method| !experimental_methods.contains(method.as_str()))
        })
        .collect();
    let new_body = filtered_arms.join(" | ");
    content = format!("{prefix}{new_body}{suffix}");
    Ok(prune_unused_type_imports(content))
}

/// Removes experimental properties from generated TypeScript type files.
fn filter_experimental_type_fields_ts(
    out_dir: &Path,
    experimental_fields: &[&'static crate::experimental_api::ExperimentalField],
) -> Result<()> {
    let mut fields_by_type_name: HashMap<String, HashSet<String>> = HashMap::new();
    for field in experimental_fields {
        fields_by_type_name
            .entry(field.type_name.to_string())
            .or_default()
            .insert(field.field_name.to_string());
    }
    if fields_by_type_name.is_empty() {
        return Ok(());
    }

    for path in ts_files_in_recursive(out_dir)? {
        let Some(type_name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(experimental_field_names) = fields_by_type_name.get(type_name) else {
            continue;
        };
        filter_experimental_fields_in_ts_file(&path, experimental_field_names)?;
    }

    Ok(())
}

fn filter_experimental_fields_in_ts_file(
    path: &Path,
    experimental_field_names: &HashSet<String>,
) -> Result<()> {
    let mut content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    content = filter_experimental_type_fields_ts_contents(content, experimental_field_names)?;
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

fn filter_experimental_type_fields_ts_contents(
    mut content: String,
    experimental_field_names: &HashSet<String>,
) -> Result<String> {
    let (open_brace, close_brace) = type_body_brace_span(&content)
        .context("cannot parse generated type with experimental fields")?;
    let inner = &content[open_brace + 1..close_brace];
    let fields = split_top_level_multi(inner, &[',', ';']);
    let filtered_fields: Vec<String> = fields
        .into_iter()
        .filter(|field| {
            let field = strip_leading_block_comments(field);
            parse_property_name(field)
                .is_none_or(|name| !experimental_field_names.contains(name.as_str()))
        })
        .collect();
    let new_inner = filtered_fields.join(", ");
    let prefix = &content[..open_brace + 1];
    let suffix = &content[close_brace..];
    content = format!("{prefix}{new_inner}{suffix}");
    Ok(prune_unused_type_imports(content))
}

fn experimental_methods() -> HashSet<&'static str> {
    EXPERIMENTAL_CLIENT_METHODS
        .iter()
        .chain(EXPERIMENTAL_SERVER_METHODS)
        .copied()
        .filter(|method| !method.is_empty())
        .collect()
}

fn filter_experimental_schema_with_metadata(
    bundle: &mut Value,
    registered_fields: &[&'static crate::experimental_api::ExperimentalField],
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
    experimental_fields: &[&'static crate::experimental_api::ExperimentalField],
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
    experimental_fields: &[&'static crate::experimental_api::ExperimentalField],
) {
    let Some(definitions) = bundle.get_mut("definitions").and_then(Value::as_object_mut) else {
        return;
    };

    filter_experimental_fields_in_definitions_map(definitions, experimental_fields);
}

fn filter_experimental_fields_in_definitions_map(
    definitions: &mut Map<String, Value>,
    experimental_fields: &[&'static crate::experimental_api::ExperimentalField],
) {
    for (def_name, def_schema) in definitions.iter_mut() {
        if is_namespace_map(def_schema) {
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

fn is_namespace_map(value: &Value) -> bool {
    let Value::Object(map) = value else {
        return false;
    };

    if map.keys().any(|key| key.starts_with('$')) {
        return false;
    }

    let looks_like_schema = map.contains_key("type")
        || map.contains_key("properties")
        || map.contains_key("anyOf")
        || map.contains_key("oneOf")
        || map.contains_key("allOf");

    !looks_like_schema && map.values().all(Value::is_object)
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
}

fn prune_experimental_methods_inner(value: &mut Value, experimental_methods: &HashSet<&str>) {
    match value {
        Value::Array(items) => {
            items.retain(|item| !is_experimental_method_variant(item, experimental_methods));
            for item in items {
                prune_experimental_methods_inner(item, experimental_methods);
            }
        }
        Value::Object(map) => {
            for entry in map.values_mut() {
                prune_experimental_methods_inner(entry, experimental_methods);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
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

fn filter_experimental_json_files(
    out_dir: &Path,
    generated_paths: &HashSet<PathBuf>,
    registered_fields: &[&'static crate::experimental_api::ExperimentalField],
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

fn experimental_method_types() -> HashSet<String> {
    let mut type_names = HashSet::new();
    collect_experimental_type_names(EXPERIMENTAL_CLIENT_METHOD_PARAM_TYPES, &mut type_names);
    collect_experimental_type_names(EXPERIMENTAL_CLIENT_METHOD_RESPONSE_TYPES, &mut type_names);
    collect_experimental_type_names(EXPERIMENTAL_CLIENT_METHOD_DEPENDENCY_TYPES, &mut type_names);
    collect_experimental_type_names(EXPERIMENTAL_SERVER_METHOD_PARAM_TYPES, &mut type_names);
    collect_experimental_type_names(EXPERIMENTAL_SERVER_METHOD_RESPONSE_TYPES, &mut type_names);
    type_names
}

fn collect_experimental_type_names(entries: &[&str], out: &mut HashSet<String>) {
    for entry in entries {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let name = trimmed.rsplit("::").next().unwrap_or(trimmed);
        if !name.is_empty() {
            out.insert(name.to_string());
        }
    }
}

fn remove_generated_type_files(
    out_dir: &Path,
    type_names: &HashSet<String>,
    extension: &str,
) -> Result<()> {
    for type_name in type_names {
        for subdir in ["", "v1", "v2"] {
            let path = if subdir.is_empty() {
                out_dir.join(format!("{type_name}.{extension}"))
            } else {
                out_dir
                    .join(subdir)
                    .join(format!("{type_name}.{extension}"))
            };
            if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to remove {}", path.display()))?;
            }
        }
    }
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

    for value in definitions.values_mut() {
        if !is_namespace_map(value) {
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

fn prune_unused_type_imports(content: String) -> String {
    let Some((declaration, _)) = type_declaration_start(&content) else {
        return content;
    };
    let mut identifiers = HashSet::new();
    let mut state = ScanState::default();
    let mut start = None;
    let scope = &content[declaration..];
    for (index, ch) in scope
        .char_indices()
        .chain(std::iter::once((scope.len(), ' ')))
    {
        if !state.in_ignored_syntax() && is_ident_char(ch) {
            start.get_or_insert(index);
        } else if let Some(start) = start.take() {
            identifiers.insert(&scope[start..index]);
        }
        state.observe(ch);
    }
    let trailing_newline = content.ends_with('\n');
    let mut lines = Vec::new();
    for line in content.lines() {
        if let Some(type_name) = parse_imported_type_name(line)
            && !identifiers.contains(type_name)
        {
            continue;
        }
        lines.push(line);
    }

    let mut rewritten = lines.join("\n");
    if trailing_newline {
        rewritten.push('\n');
    }
    rewritten
}

fn parse_imported_type_name(line: &str) -> Option<&str> {
    let line = line.trim();
    let rest = line.strip_prefix("import type {")?;
    let (type_name, _) = rest.split_once("} from ")?;
    let type_name = type_name.trim();
    if type_name.is_empty() || type_name.contains(',') || type_name.contains(" as ") {
        return None;
    }
    Some(type_name)
}

fn json_files_in_recursive(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
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

// Restrict parsing to a real generated declaration, ignoring examples in comments.
fn type_declaration_start(content: &str) -> Option<(usize, bool)> {
    let mut state = ScanState::default();
    for (index, ch) in content.char_indices() {
        if !state.in_ignored_syntax()
            && state.depth.is_top_level()
            && (index == 0
                || !content[..index]
                    .chars()
                    .next_back()
                    .is_some_and(is_ident_char))
        {
            if content[index..].starts_with("export type ") {
                return Some((index, false));
            }
            if content[index..].starts_with("export interface ") {
                return Some((index, true));
            }
        }
        state.observe(ch);
    }
    None
}

fn top_level_char(input: &str, target: char) -> Option<usize> {
    let mut state = ScanState::default();
    for (index, ch) in input.char_indices() {
        if !state.in_ignored_syntax() && state.depth.is_top_level() && ch == target {
            return Some(index);
        }
        state.observe(ch);
    }
    None
}

fn split_type_alias(content: &str) -> Option<(String, String, String)> {
    let (start, interface) = type_declaration_start(content)?;
    if interface {
        return None;
    }
    let eq_index = start + top_level_char(&content[start..], '=')?;
    let semi_index = eq_index + 1 + top_level_char(&content[eq_index + 1..], ';')?;
    Some((
        content[..eq_index + 1].to_string(),
        content[eq_index + 1..semi_index].to_string(),
        content[semi_index..].to_string(),
    ))
}

fn type_body_brace_span(content: &str) -> Option<(usize, usize)> {
    let (start, interface) = type_declaration_start(content)?;
    let body_start = if interface {
        start + "export interface ".len()
    } else {
        start + top_level_char(&content[start..], '=')? + 1
    };
    let (open, close) = find_top_level_brace_span(&content[body_start..])?;
    Some((body_start + open, body_start + close))
}

fn find_top_level_brace_span(input: &str) -> Option<(usize, usize)> {
    let mut state = ScanState::default();
    let mut open_index = None;
    for (index, ch) in input.char_indices() {
        if !state.in_ignored_syntax() && ch == '{' && state.depth.is_top_level() {
            open_index = Some(index);
        }
        state.observe(ch);
        if !state.in_ignored_syntax()
            && ch == '}'
            && state.depth.is_top_level()
            && let Some(open) = open_index
        {
            return Some((open, index));
        }
    }
    None
}

fn split_top_level(input: &str, delimiter: char) -> Vec<String> {
    split_top_level_multi(input, &[delimiter])
}

fn split_top_level_multi(input: &str, delimiters: &[char]) -> Vec<String> {
    let mut state = ScanState::default();
    let mut start = 0usize;
    let mut parts = Vec::new();
    for (index, ch) in input.char_indices() {
        if !state.in_ignored_syntax() && state.depth.is_top_level() && delimiters.contains(&ch) {
            let part = input[start..index].trim();
            if !part.is_empty() {
                parts.push(part.to_string());
            }
            start = index + ch.len_utf8();
        }
        state.observe(ch);
    }
    let tail = input[start..].trim();
    if !tail.is_empty() {
        parts.push(tail.to_string());
    }
    parts
}

fn extract_method_from_arm(arm: &str) -> Option<String> {
    let (open, close) = find_top_level_brace_span(arm)?;
    let inner = &arm[open + 1..close];
    for field in split_top_level(inner, ',') {
        let Some((name, value)) = parse_property(field.as_str()) else {
            continue;
        };
        if name != "method" {
            continue;
        }
        let value = value.trim_start();
        let (literal, _) = parse_string_literal(value)?;
        return Some(literal);
    }
    None
}

fn parse_property(input: &str) -> Option<(String, &str)> {
    let name = parse_property_name(input)?;
    let colon_index = input.find(':')?;
    Some((name, input[colon_index + 1..].trim_start()))
}

fn strip_leading_block_comments(input: &str) -> &str {
    let mut rest = input.trim_start();
    loop {
        let Some(after_prefix) = rest.strip_prefix("/*") else {
            return rest;
        };
        let Some(end_rel) = after_prefix.find("*/") else {
            return rest;
        };
        rest = after_prefix[end_rel + 2..].trim_start();
    }
}

fn parse_property_name(input: &str) -> Option<String> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((literal, consumed)) = parse_string_literal(trimmed) {
        let rest = trimmed[consumed..].trim_start();
        let rest = rest.strip_prefix('?').unwrap_or(rest).trim_start();
        if rest.starts_with(':') {
            return Some(literal);
        }
        return None;
    }

    let mut end = 0usize;
    for (index, ch) in trimmed.char_indices() {
        if !is_ident_char(ch) {
            break;
        }
        end = index + ch.len_utf8();
    }
    if end == 0 {
        return None;
    }
    let name = &trimmed[..end];
    let rest = trimmed[end..].trim_start();
    let rest = if let Some(stripped) = rest.strip_prefix('?') {
        stripped.trim_start()
    } else {
        rest
    };
    if rest.starts_with(':') {
        return Some(name.to_string());
    }
    None
}

fn parse_string_literal(input: &str) -> Option<(String, usize)> {
    let mut chars = input.char_indices();
    let (start_index, quote) = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let mut escape = false;
    for (index, ch) in chars {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' {
            escape = true;
            continue;
        }
        if ch == quote {
            let literal = input[start_index + 1..index].to_string();
            let consumed = index + ch.len_utf8();
            return Some((literal, consumed));
        }
    }
    None
}

fn is_ident_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

#[derive(Default)]
struct ScanState {
    depth: Depth,
    string_delim: Option<char>,
    escape: bool,
    block_comment: bool,
    line_comment: bool,
    previous_char: Option<char>,
}

impl ScanState {
    fn observe(&mut self, ch: char) {
        if self.line_comment {
            if ch == '\n' {
                self.line_comment = false;
            }
            self.previous_char = Some(ch);
            return;
        }

        if self.block_comment {
            if self.previous_char == Some('*') && ch == '/' {
                self.block_comment = false;
                self.previous_char = None;
            } else {
                self.previous_char = Some(ch);
            }
            return;
        }

        if let Some(delim) = self.string_delim {
            if self.escape {
                self.escape = false;
                self.previous_char = Some(ch);
                return;
            }
            if ch == '\\' {
                self.escape = true;
                self.previous_char = Some(ch);
                return;
            }
            if ch == delim {
                self.string_delim = None;
            }
            self.previous_char = Some(ch);
            return;
        }

        if self.previous_char == Some('/') && ch == '/' {
            self.line_comment = true;
            self.previous_char = Some(ch);
            return;
        }

        if self.previous_char == Some('/') && ch == '*' {
            self.block_comment = true;
            self.previous_char = Some(ch);
            return;
        }

        match ch {
            '"' | '\'' => {
                self.string_delim = Some(ch);
            }
            '{' => self.depth.brace += 1,
            '}' => self.depth.brace = (self.depth.brace - 1).max(0),
            '[' => self.depth.bracket += 1,
            ']' => self.depth.bracket = (self.depth.bracket - 1).max(0),
            '(' => self.depth.paren += 1,
            ')' => self.depth.paren = (self.depth.paren - 1).max(0),
            '<' => self.depth.angle += 1,
            '>' if self.depth.angle > 0 => {
                self.depth.angle -= 1;
            }
            _ => {}
        }
        self.previous_char = Some(ch);
    }

    fn in_ignored_syntax(&self) -> bool {
        self.string_delim.is_some() || self.block_comment || self.line_comment
    }
}

#[derive(Default)]
struct Depth {
    brace: i32,
    bracket: i32,
    paren: i32,
    angle: i32,
}

impl Depth {
    fn is_top_level(&self) -> bool {
        self.brace == 0 && self.bracket == 0 && self.paren == 0 && self.angle == 0
    }
}

fn build_schema_bundle(schemas: Vec<GeneratedSchema>) -> Result<Value> {
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
                        insert_into_namespace(&mut definitions, ns, def_name.clone(), def_schema)?;
                    }
                } else {
                    insert_definition(&mut definitions, def_name, def_schema, "root")?;
                }
            }
        }

        if let Some(ref ns) = namespace {
            insert_into_namespace(&mut definitions, ns, logical_name.clone(), value)?;
        } else {
            insert_definition(&mut definitions, logical_name, value, "root")?;
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
fn build_flat_v2_schema(bundle: &Value) -> Result<Value> {
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
            insert_definition(&mut flat_definitions, name, schema, "flat v2")?;
        }
    }

    for (name, mut schema) in shared_definitions {
        rewrite_ref_prefix(&mut schema, "#/definitions/v2/", "#/definitions/");
        insert_definition(&mut flat_definitions, name, schema, "flat v2")?;
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
    match value {
        Value::Object(obj) => {
            if let Some(Value::String(reference)) = obj.get("$ref")
                && let Some(name) = reference.strip_prefix("#/definitions/")
                && !reference.starts_with("#/definitions/v2/")
            {
                refs.insert(name.to_string());
            }
            for child in obj.values() {
                collect_non_v2_refs_inner(child, refs);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_non_v2_refs_inner(child, refs);
            }
        }
        _ => {}
    }
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
    match value {
        Value::Object(obj) => {
            if let Some(Value::String(reference)) = obj.get_mut("$ref") {
                *reference = reference.replace(prefix, replacement);
            }
            for child in obj.values_mut() {
                rewrite_ref_prefix(child, prefix, replacement);
            }
        }
        Value::Array(items) => {
            for child in items {
                rewrite_ref_prefix(child, prefix, replacement);
            }
        }
        _ => {}
    }
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
    match value {
        Value::Object(obj) => {
            if let Some(Value::String(reference)) = obj.get("$ref")
                && reference.starts_with(prefix)
            {
                return Some(reference.clone());
            }
            obj.values()
                .find_map(|child| first_ref_with_prefix(child, prefix))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| first_ref_with_prefix(child, prefix)),
        _ => None,
    }
}

fn ensure_referenced_definitions_present(schema: &Value, label: &str) -> Result<()> {
    let definitions = schema
        .get("definitions")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("expected definitions map in {label} schema"))?;
    let mut missing = HashSet::new();
    collect_missing_definitions(schema, definitions, &mut missing);
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

fn collect_missing_definitions(
    value: &Value,
    definitions: &Map<String, Value>,
    missing: &mut HashSet<String>,
) {
    match value {
        Value::Object(obj) => {
            if let Some(Value::String(reference)) = obj.get("$ref")
                && let Some(name) = reference.strip_prefix("#/definitions/")
            {
                let name = name.split('/').next().unwrap_or(name);
                if !definitions.contains_key(name) {
                    missing.insert(name.to_string());
                }
            }
            for child in obj.values() {
                collect_missing_definitions(child, definitions, missing);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_missing_definitions(child, definitions, missing);
            }
        }
        _ => {}
    }
}

fn insert_into_namespace(
    definitions: &mut Map<String, Value>,
    namespace: &str,
    name: String,
    schema: Value,
) -> Result<()> {
    let entry = definitions
        .entry(namespace.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    match entry {
        Value::Object(map) => {
            insert_definition(map, name, schema, &format!("namespace `{namespace}`"))
        }
        _ => Err(anyhow!("expected namespace {namespace} to be an object")),
    }
}

fn insert_definition(
    definitions: &mut Map<String, Value>,
    name: String,
    schema: Value,
    location: &str,
) -> Result<()> {
    if let Some(existing) = definitions.get_mut(&name) {
        if existing == &schema {
            return Ok(());
        }

        // Schemars includes the dialect and type title on standalone roots but
        // omits them on the same type embedded in another schema's definitions.
        // Compare that representation consistently without masking different
        // dialects, titles, or validation keywords.
        let normalize = |value: &Value| {
            let mut value = value.clone();
            if let Some(map) = value.as_object_mut() {
                if map.get("$schema").and_then(Value::as_str)
                    == Some("http://json-schema.org/draft-07/schema#")
                {
                    map.remove("$schema");
                }
                if map.get("title").and_then(Value::as_str) == Some(name.as_str()) {
                    map.remove("title");
                }
            }
            value
        };
        if normalize(existing) == normalize(&schema) {
            // Root definitions historically kept the last emitted representation;
            // namespace and flattened definitions kept the first. Preserve those
            // annotations without allowing incompatible schemas to overwrite.
            if location == "root" {
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

pub(crate) fn generate_json_schema<T>(name: &str) -> Result<GeneratedSchema>
where
    T: JsonSchema,
{
    let file_stem = name.trim();
    let (raw_namespace, logical_name) = split_namespace(file_stem);
    let include_in_json_codegen =
        raw_namespace != Some("v1") || JSON_V1_ALLOWLIST.contains(&logical_name);
    let schema = schema_for!(T);
    let mut schema_value = serde_json::to_value(schema)?;
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

fn json_schema_relative_path(schema: &GeneratedSchema) -> Option<PathBuf> {
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

fn write_json_schema_with_return<T: JsonSchema>(
    out_dir: &Path,
    name: &str,
) -> Result<GeneratedSchema> {
    let schema = generate_json_schema::<T>(name)?;
    if let Some(relative_path) = json_schema_relative_path(&schema) {
        let path = out_dir.join(relative_path);
        if let Some(parent) = path.parent() {
            ensure_dir(parent)?;
        }
        write_pretty_json(path, &schema.value)?;
    }
    Ok(schema)
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

fn collect_local_definition_refs_excluding_maps(
    value: &Value,
    defs_key: &str,
    queue: &mut Vec<String>,
    reachable: &mut HashSet<String>,
) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == defs_key || key == "$defs" || key == "definitions" {
                    continue;
                }
                collect_local_definition_refs_excluding_maps(child, defs_key, queue, reachable);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_local_definition_refs_excluding_maps(child, defs_key, queue, reachable);
            }
        }
        _ => {}
    }
    collect_local_definition_ref_here(value, defs_key, queue, reachable);
}

fn collect_local_definition_refs(
    value: &Value,
    defs_key: &str,
    queue: &mut Vec<String>,
    reachable: &mut HashSet<String>,
) {
    collect_local_definition_ref_here(value, defs_key, queue, reachable);
    match value {
        Value::Object(map) => {
            for child in map.values() {
                collect_local_definition_refs(child, defs_key, queue, reachable);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_local_definition_refs(child, defs_key, queue, reachable);
            }
        }
        _ => {}
    }
}

fn collect_local_definition_ref_here(
    value: &Value,
    defs_key: &str,
    queue: &mut Vec<String>,
    reachable: &mut HashSet<String>,
) {
    let Some(reference) = value
        .as_object()
        .and_then(|obj| obj.get("$ref"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let Some(name) = reference.strip_prefix(&format!("#/{defs_key}/")) else {
        return;
    };
    let name = name.split('/').next().unwrap_or(name);
    if reachable.insert(name.to_string()) {
        queue.push(name.to_string());
    }
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

pub(crate) fn write_json_schema<T>(out_dir: &Path, name: &str) -> Result<GeneratedSchema>
where
    T: JsonSchema,
{
    write_json_schema_with_return::<T>(out_dir, name)
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

fn pretty_json_value_bytes(mut value: Value) -> Result<Vec<u8>> {
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

/// Recursively rewrite $ref values that point at "#/definitions/..." so that
/// they point to a namespaced location under the bundle.
fn rewrite_refs_to_namespace(value: &mut Value, ns: &str) {
    match value {
        Value::Object(obj) => {
            if let Some(Value::String(r)) = obj.get_mut("$ref")
                && let Some(suffix) = r.strip_prefix("#/definitions/")
            {
                let prefix = format!("{ns}/");
                if !suffix.starts_with(&prefix) {
                    *r = format!("#/definitions/{ns}/{suffix}");
                }
            }
            for v in obj.values_mut() {
                rewrite_refs_to_namespace(v, ns);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                rewrite_refs_to_namespace(v, ns);
            }
        }
        _ => {}
    }
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
    match value {
        Value::Object(obj) => {
            if let Some(Value::String(reference)) = obj.get_mut("$ref")
                && let Some(suffix) = reference.strip_prefix("#/definitions/")
            {
                let (name, tail) = suffix
                    .split_once('/')
                    .map_or((suffix, None), |(name, tail)| (name, Some(tail)));
                if let Some(ns) = namespace_for_definition(name, types) {
                    let tail = tail.map_or(String::new(), |rest| format!("/{rest}"));
                    *reference = format!("#/definitions/{ns}/{name}{tail}");
                }
            }
            for v in obj.values_mut() {
                rewrite_refs_to_known_namespaces(v, types);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                rewrite_refs_to_known_namespaces(v, types);
            }
        }
        _ => {}
    }
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

fn annotate_schema(value: &mut Value, base: Option<&str>) {
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
            _ => annotate_schema(child, base),
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

fn ensure_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create output directory {}", dir.display()))
}

fn prepend_header_if_missing(path: &Path) -> Result<()> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("Failed to open {} for reading", path.display()))?
        .take(MAX_TYPESCRIPT_HEADER_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    if bytes.len() as u64 > MAX_TYPESCRIPT_HEADER_INPUT_BYTES {
        return Err(anyhow!(
            "TypeScript file {} exceeds the {MAX_TYPESCRIPT_HEADER_INPUT_BYTES}-byte header processing limit",
            path.display()
        ));
    }
    let content = String::from_utf8(bytes)
        .with_context(|| format!("Failed to read {} as UTF-8", path.display()))?;

    if content.starts_with(GENERATED_TS_HEADER) {
        return Ok(());
    }

    let mut f = fs::File::create(path)
        .with_context(|| format!("Failed to open {} for writing", path.display()))?;
    f.write_all(GENERATED_TS_HEADER.as_bytes())
        .with_context(|| format!("Failed to write header to {}", path.display()))?;
    f.write_all(content.as_bytes())
        .with_context(|| format!("Failed to write content to {}", path.display()))?;
    Ok(())
}

fn ts_files_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in
        fs::read_dir(dir).with_context(|| format!("Failed to read dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension() == Some(OsStr::new("ts")) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn ts_files_in_recursive(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in
            fs::read_dir(&d).with_context(|| format!("Failed to read dir {}", d.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() && path.extension() == Some(OsStr::new("ts")) {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn trim_trailing_whitespace_in_ts_files(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let trimmed = trim_trailing_line_whitespace(&content);
        if trimmed != content {
            fs::write(path, trimmed)
                .with_context(|| format!("Failed to write {}", path.display()))?;
        }
    }
    Ok(())
}

pub(crate) fn trim_trailing_line_whitespace(content: &str) -> String {
    let mut trimmed = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        if let Some(line_without_newline) = line.strip_suffix('\n') {
            trimmed.push_str(line_without_newline.trim_end_matches([' ', '\t']));
            trimmed.push('\n');
        } else {
            trimmed.push_str(line.trim_end_matches([' ', '\t']));
        }
    }
    trimmed
}

/// Generate an index.ts file that re-exports all generated types.
/// This allows consumers to import all types from a single file.
fn generate_index_ts(out_dir: &Path) -> Result<PathBuf> {
    let content = generated_index_ts_with_header(index_ts_entries(
        &ts_files_in(out_dir)?
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>(),
        ts_files_in(&out_dir.join("v2"))
            .map(|v| !v.is_empty())
            .unwrap_or(false),
    ));

    let index_path = out_dir.join("index.ts");
    let mut f = fs::File::create(&index_path)
        .with_context(|| format!("Failed to create {}", index_path.display()))?;
    f.write_all(content.as_bytes())
        .with_context(|| format!("Failed to write {}", index_path.display()))?;
    Ok(index_path)
}


fn generated_index_ts_with_header(content: String) -> String {
    let mut with_header = String::with_capacity(GENERATED_TS_HEADER.len() + content.len());
    with_header.push_str(GENERATED_TS_HEADER);
    with_header.push_str(&content);
    with_header
}

fn index_ts_entries(paths: &[&Path], has_v2_ts: bool) -> String {
    let mut stems: Vec<String> = paths
        .iter()
        .filter(|path| path.extension() == Some(OsStr::new("ts")))
        .filter_map(|path| {
            let stem = path.file_stem()?.to_string_lossy().into_owned();
            if stem == "index" { None } else { Some(stem) }
        })
        .filter(|stem| stem != "EventMsg")
        .collect();
    stems.sort();
    stems.dedup();

    let mut entries = String::new();
    for name in stems {
        let _ = writeln!(entries, "export type {{ {name} }} from \"./{name}\";");
    }
    if has_v2_ts {
        entries.push_str("export * as v2 from \"./v2\";\n");
    }
    entries
}

