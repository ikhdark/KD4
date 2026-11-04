mod json_schema;
mod schema_bundle;
mod typescript;

use json_schema::filter_experimental_json_files;
use json_schema::filter_experimental_schema_with_metadata;
use json_schema::json_schema_relative_path;
use json_schema::normalize_json_schema;
use json_schema::normalize_raw_schemas;
use json_schema::pretty_json_value_bytes;
use json_schema::write_json_schema;
use schema_bundle::build_flat_v2_schema;
use schema_bundle::build_schema_bundle;
use typescript::filter_experimental_ts;
use typescript::generate_index_ts;
use typescript::prepend_header_if_missing;
use typescript::prettier_file_arguments;
use typescript::prettier_file_batches;
use typescript::trim_trailing_whitespace_in_ts_files;
use typescript::ts_files_in_recursive;

use codex_app_server_protocol_types as types;
use types::ClientNotification;
use types::ClientRequest;
use types::ServerNotification;
use types::ServerRequest;
use types::experimental_fields;
use types::schema_support::export_client_notification_schemas;
use types::schema_support::export_client_param_schemas;
use types::schema_support::export_client_response_schemas;
use types::export_client_responses;
use types::schema_support::export_server_notification_schemas;
use types::schema_support::export_server_param_schemas;
use types::schema_support::export_server_response_schemas;
use types::export_server_responses;
use types::schema_support::EXPERIMENTAL_CLIENT_METHOD_PARAM_TYPES;
use types::schema_support::EXPERIMENTAL_CLIENT_METHOD_RESPONSE_TYPES;
use types::schema_support::EXPERIMENTAL_CLIENT_METHODS;
use types::schema_support::EXPERIMENTAL_SERVER_METHOD_PARAM_TYPES;
use types::schema_support::EXPERIMENTAL_SERVER_METHOD_RESPONSE_TYPES;
use types::schema_support::EXPERIMENTAL_SERVER_METHODS;
use types::schema_support::RawSchemaEntry;
use types::schema_support::generate_raw_json_schema;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use codex_protocol::protocol::RolloutLine;
use schemars::JsonSchema;
use schemars::schema_for;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use ts_rs::TS;

pub(crate) const GENERATED_TS_HEADER: &str = "// GENERATED CODE! DO NOT MODIFY BY HAND!\n\n";
const JSON_V1_ALLOWLIST: &[&str] = &["InitializeParams", "InitializeResponse"];
const EXPERIMENTAL_CLIENT_METHOD_DEPENDENCY_TYPES: &[&str] = &[
    "EnvironmentShellInfo",
    "RemoteControlClient",
    "RemoteControlClientsListOrder",
    "ThreadBackgroundTerminal",
];

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

type JsonSchemaEmitter = fn() -> Result<RawSchemaEntry>;

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
    ensure_fresh_output_dir(out_dir, "ts")?;
    let v2_out_dir = out_dir.join("v2");
    ensure_dir(out_dir)?;
    ensure_dir(&v2_out_dir)?;

    let config = ts_rs::Config::from_env().with_out_dir(out_dir).with_large_int("number");
    ClientRequest::export_all(&config)?;
    export_client_responses(out_dir)?;
    ClientNotification::export_all(&config)?;

    ServerRequest::export_all(&config)?;
    export_server_responses(out_dir)?;
    ServerNotification::export_all(&config)?;

    types::OverloadErrorData::export_all(&config)?;
    types::PluginRemoteErrorData::export_all(&config)?;
    types::ThreadErrorData::export_all(&config)?;

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

pub fn generate_internal_json_schema(out_dir: &Path) -> Result<()> {
    ensure_dir(out_dir)?;
    write_json_schema::<RolloutLine>(out_dir, "RolloutLine")?;
    Ok(())
}

pub fn generate_json_with_experimental(out_dir: &Path, experimental_api: bool) -> Result<()> {
    ensure_fresh_output_dir(out_dir, "json")?;
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

/// Public exports own a fresh tree; refusing reused output avoids retired schemas
/// and prevents postprocessing unrelated files supplied by the caller.
fn ensure_fresh_output_dir(out_dir: &Path, extension: &str) -> Result<()> {
    if out_dir.exists() {
        let mut directories = vec![out_dir.to_path_buf()];
        while let Some(directory) = directories.pop() {
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let kind = entry.file_type()?;
                let path = entry.path();
                anyhow::ensure!(!kind.is_symlink(), "linked schema output is not supported: {}", path.display());
                if kind.is_dir() {
                    directories.push(path);
                } else {
                    anyhow::ensure!(
                        path.extension().and_then(|value| value.to_str()) != Some(extension),
                        "schema output tree already contains {extension} files: {}", path.display()
                    );
                }
            }
        }
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
        || generate_raw_json_schema::<types::RequestId>("RequestId"),
        || generate_raw_json_schema::<types::JSONRPCMessage>("JSONRPCMessage"),
        || generate_raw_json_schema::<types::JSONRPCRequest>("JSONRPCRequest"),
        || generate_raw_json_schema::<types::JSONRPCNotification>("JSONRPCNotification"),
        || generate_raw_json_schema::<types::JSONRPCResponse>("JSONRPCResponse"),
        || generate_raw_json_schema::<types::JSONRPCError>("JSONRPCError"),
        || generate_raw_json_schema::<types::JSONRPCErrorError>("JSONRPCErrorError"),
        || generate_raw_json_schema::<types::OverloadErrorData>("OverloadErrorData"),
        || generate_raw_json_schema::<types::PluginRemoteErrorData>("v2::PluginRemoteErrorData"),
        || generate_raw_json_schema::<types::ThreadErrorData>("v2::ThreadErrorData"),
        || generate_raw_json_schema::<types::ClientRequest>("ClientRequest"),
        || generate_raw_json_schema::<types::ServerRequest>("ServerRequest"),
        || generate_raw_json_schema::<types::ClientNotification>("ClientNotification"),
        || generate_raw_json_schema::<types::ServerNotification>("ServerNotification"),
    ];

    let mut schemas: Vec<GeneratedSchema> = Vec::new();
    for emit in &envelope_emitters {
        let entry = emit()?;
        schemas.push(normalize_json_schema(entry.name, entry.schema)?);
    }

    schemas.extend(normalize_raw_schemas(export_client_param_schemas()?)?);
    schemas.extend(normalize_raw_schemas(export_client_response_schemas()?)?);
    schemas.extend(normalize_raw_schemas(export_server_param_schemas()?)?);
    schemas.extend(normalize_raw_schemas(export_server_response_schemas()?)?);
    schemas.extend(normalize_raw_schemas(export_client_notification_schemas()?)?);
    schemas.extend(normalize_raw_schemas(export_server_notification_schemas()?)?);
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

fn experimental_methods() -> HashSet<&'static str> {
    EXPERIMENTAL_CLIENT_METHODS
        .iter()
        .chain(EXPERIMENTAL_SERVER_METHODS)
        .copied()
        .filter(|method| !method.is_empty())
        .collect()
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


pub(crate) fn generate_json_schema<T>(name: &str) -> Result<GeneratedSchema>
where
    T: JsonSchema,
{
    let schema = schema_for!(T);
    normalize_json_schema(name, serde_json::to_value(schema)?)
}

fn ensure_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create output directory {}", dir.display()))
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

