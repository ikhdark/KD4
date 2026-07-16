use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use tempfile::NamedTempFile;
use thiserror::Error;
use walkdir::WalkDir;

const CACHE_FORMAT_VERSION: u64 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CargoMetadataRequest {
    pub repository_root: PathBuf,
    pub workspace_root: PathBuf,
    pub cargo: OsString,
    pub rustc: OsString,
    pub extra_args: Vec<OsString>,
    pub environment_mode: String,
    pub no_cache: bool,
    pub cache_readonly: bool,
}

impl CargoMetadataRequest {
    pub fn for_repository(repository_root: &Path) -> Self {
        Self {
            repository_root: repository_root.to_path_buf(),
            workspace_root: repository_root.join("codex-rs"),
            cargo: std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")),
            rustc: std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc")),
            extra_args: Vec::new(),
            environment_mode: "inherited".to_string(),
            no_cache: false,
            cache_readonly: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CargoCacheDisposition {
    Hit,
    MissStored,
    MissReadonly,
    Bypassed { reasons: Vec<String> },
}

#[derive(Clone, Debug)]
pub struct CargoMetadataResult {
    pub metadata: Value,
    pub fingerprint: Option<String>,
    pub disposition: CargoCacheDisposition,
}

#[derive(Debug, Error)]
pub enum CargoCacheError {
    #[error("failed to canonicalize {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Cargo metadata failed: {0}")]
    Metadata(String),
    #[error("Cargo metadata was not exactly one JSON value: {0}")]
    MetadataJson(#[from] serde_json::Error),
    #[error("cache I/O failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct InputRecord {
    label: String,
    path_bytes_base64: String,
    file_type: String,
    present: bool,
    length: u64,
    sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedInputKind {
    File,
    Directory,
    Either,
}

#[derive(Clone, Debug, Default)]
struct Inventory {
    logical: BTreeMap<String, Vec<u8>>,
    files: BTreeMap<Vec<u8>, InputRecord>,
    complete: bool,
    reasons: BTreeSet<String>,
    discovery_roots: BTreeSet<PathBuf>,
    manifests: BTreeSet<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CacheEntry {
    cache_format_version: u64,
    seed: String,
    fingerprint: String,
    metadata: Value,
    previous_manifest_paths: Vec<NativePathWire>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct NativePathWire {
    bytes_base64: String,
}

impl NativePathWire {
    fn from_path(path: &Path) -> Self {
        Self {
            bytes_base64: base64_encode(&native_path_bytes(path)),
        }
    }

    fn to_path(&self) -> Option<PathBuf> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.bytes_base64)
            .ok()?;
        native_path_from_bytes(bytes)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CacheIndex {
    cache_format_version: u64,
    fingerprint: String,
}

pub fn load_cargo_metadata(
    request: &CargoMetadataRequest,
) -> Result<CargoMetadataResult, CargoCacheError> {
    let mut request = request.clone();
    request.repository_root = canonical(&request.repository_root)?;
    request.workspace_root = canonical(&request.workspace_root)?;
    let seed = invocation_seed(&request);
    let cache_root = request
        .repository_root
        .join(".codex/verify-local/cargo-graph-v1");
    let index_path = cache_root.join("index").join(format!("{seed}.json"));

    if !request.no_cache
        && let Some(entry) = read_candidate_entry(&cache_root, &index_path)?
    {
        let previous = entry
            .previous_manifest_paths
            .iter()
            .map(NativePathWire::to_path)
            .collect::<Option<BTreeSet<_>>>();
        if let Some(previous) = previous {
            let inventory = build_inventory(&request, &previous);
            let fingerprint = complete_fingerprint(&request, &inventory);
            if inventory.complete && entry.fingerprint == fingerprint {
                return Ok(CargoMetadataResult {
                    metadata: entry.metadata,
                    fingerprint: Some(fingerprint),
                    disposition: CargoCacheDisposition::Hit,
                });
            }
        }
    }

    let initial = build_inventory(&request, &BTreeSet::new());
    let eligible_before_run = initial.complete && initial.reasons.is_empty();
    let locked = request.workspace_root.join("Cargo.lock").is_file();
    let metadata = run_metadata(&request, locked)?;
    let (previous, metadata_inventory_error) = match metadata_manifest_paths(&metadata) {
        Ok(previous) => (previous, None),
        Err(error) => (BTreeSet::new(), Some(error)),
    };
    let mut inventory = build_inventory(&request, &previous);
    if let Some(error) = metadata_inventory_error {
        incomplete(&mut inventory, error);
    }
    let fingerprint = complete_fingerprint(&request, &inventory);
    let mut reasons = initial
        .reasons
        .union(&inventory.reasons)
        .cloned()
        .collect::<Vec<_>>();
    if !eligible_before_run {
        reasons.push("pre-metadata dependency state was not closed-world".to_string());
    }
    if request.no_cache {
        reasons.push("cache disabled by request".to_string());
    }
    if !eligible_before_run || !inventory.complete || request.no_cache {
        return Ok(CargoMetadataResult {
            metadata,
            fingerprint: Some(fingerprint),
            disposition: CargoCacheDisposition::Bypassed { reasons },
        });
    }
    if request.cache_readonly {
        return Ok(CargoMetadataResult {
            metadata,
            fingerprint: Some(fingerprint),
            disposition: CargoCacheDisposition::MissReadonly,
        });
    }

    let entry = CacheEntry {
        cache_format_version: CACHE_FORMAT_VERSION,
        seed: seed.clone(),
        fingerprint: fingerprint.clone(),
        metadata: metadata.clone(),
        previous_manifest_paths: previous
            .iter()
            .map(|path| NativePathWire::from_path(path))
            .collect(),
    };
    store_entry(&cache_root, &index_path, &entry)?;
    Ok(CargoMetadataResult {
        metadata,
        fingerprint: Some(fingerprint),
        disposition: CargoCacheDisposition::MissStored,
    })
}

fn build_inventory(request: &CargoMetadataRequest, previous: &BTreeSet<PathBuf>) -> Inventory {
    let mut inventory = Inventory {
        complete: true,
        ..Inventory::default()
    };
    logical(
        &mut inventory,
        "cache-format",
        CACHE_FORMAT_VERSION.to_be_bytes(),
    );
    logical_os(&mut inventory, "cargo-argv-0", &request.cargo);
    for (index, argument) in metadata_args(request, true).iter().enumerate() {
        logical_os(
            &mut inventory,
            &format!("cargo-argv-{}", index + 1),
            argument,
        );
    }
    logical_os(&mut inventory, "rustc", &request.rustc);
    logical(
        &mut inventory,
        "environment-mode",
        request.environment_mode.as_bytes(),
    );
    logical_path(&mut inventory, "repository-root", &request.repository_root);
    logical_path(&mut inventory, "workspace-root", &request.workspace_root);
    validate_metadata_topology_args(&mut inventory, &request.extra_args);

    for (key, value) in relevant_environment() {
        logical_os(
            &mut inventory,
            &format!("env:raw:{}", base64_encode(&native_os_bytes(&key))),
            &value,
        );
    }
    let cargo_home = effective_cargo_home(&request.workspace_root);
    if let Some(home) = &cargo_home {
        logical_path(&mut inventory, "cargo-home", home);
    } else {
        incomplete(
            &mut inventory,
            "effective CARGO_HOME could not be determined",
        );
    }

    let cargo_path = resolve_executable(&request.cargo);
    let rustc_path = resolve_executable(&request.rustc);
    match cargo_path {
        Some(path) => record_file(
            &mut inventory,
            "cargo-executable",
            &path,
            true,
            ExpectedInputKind::File,
        ),
        None => incomplete(
            &mut inventory,
            "selected Cargo executable could not be resolved",
        ),
    }
    match rustc_path {
        Some(path) => record_file(
            &mut inventory,
            "rustc-executable",
            &path,
            true,
            ExpectedInputKind::File,
        ),
        None => incomplete(
            &mut inventory,
            "selected rustc executable could not be resolved",
        ),
    }
    record_command_version(
        &mut inventory,
        "cargo-version",
        &request.cargo,
        &[OsString::from("--version"), OsString::from("--verbose")],
        &request.workspace_root,
    );
    record_command_version(
        &mut inventory,
        "rustc-version",
        &request.rustc,
        &[OsString::from("--version"), OsString::from("--verbose")],
        &request.workspace_root,
    );
    record_git_identity(&mut inventory, &request.repository_root);

    for ancestor in request.workspace_root.ancestors() {
        record_file(
            &mut inventory,
            "ancestor-cargo-config",
            &ancestor.join(".cargo/config"),
            false,
            ExpectedInputKind::File,
        );
        record_file(
            &mut inventory,
            "ancestor-cargo-config-toml",
            &ancestor.join(".cargo/config.toml"),
            false,
            ExpectedInputKind::File,
        );
    }
    if let Some(cargo_home) = cargo_home {
        record_file(
            &mut inventory,
            "cargo-home-config",
            &cargo_home.join("config"),
            false,
            ExpectedInputKind::File,
        );
        record_file(
            &mut inventory,
            "cargo-home-config-toml",
            &cargo_home.join("config.toml"),
            false,
            ExpectedInputKind::File,
        );
    }
    for (label, path) in [
        (
            "workspace-manifest",
            request.workspace_root.join("Cargo.toml"),
        ),
        ("workspace-lock", request.workspace_root.join("Cargo.lock")),
        (
            "rust-toolchain",
            request.repository_root.join("rust-toolchain"),
        ),
        (
            "rust-toolchain-toml",
            request.repository_root.join("rust-toolchain.toml"),
        ),
        (
            "workspace-rust-toolchain",
            request.workspace_root.join("rust-toolchain"),
        ),
        (
            "workspace-rust-toolchain-toml",
            request.workspace_root.join("rust-toolchain.toml"),
        ),
        (
            "verifier-rules",
            request
                .repository_root
                .join("scripts/verify_local_rules.toml"),
        ),
    ] {
        let required = matches!(
            label,
            "workspace-manifest" | "workspace-lock" | "verifier-rules"
        );
        record_file(
            &mut inventory,
            label,
            &path,
            required,
            ExpectedInputKind::File,
        );
    }

    let root_manifest = request.workspace_root.join("Cargo.toml");
    inventory.manifests.insert(root_manifest.clone());
    discover_workspace(&mut inventory, request, &root_manifest);
    let mut manifests = inventory.manifests.clone();
    manifests.extend(previous.iter().cloned());
    discover_path_dependencies(&mut inventory, &mut manifests);
    for manifest in manifests {
        record_file(
            &mut inventory,
            "package-manifest",
            &manifest,
            true,
            ExpectedInputKind::File,
        );
        inventory.manifests.insert(manifest.clone());
        inventory_targets(&mut inventory, &manifest);
    }
    validate_lockfile(&mut inventory, &request.workspace_root.join("Cargo.lock"));
    inventory
}

fn discover_workspace(
    inventory: &mut Inventory,
    request: &CargoMetadataRequest,
    root_manifest: &Path,
) {
    let value = match read_toml(root_manifest) {
        Ok(value) => value,
        Err(error) => {
            incomplete(inventory, error);
            return;
        }
    };
    let workspace = match value.get("workspace").and_then(toml::Value::as_table) {
        Some(workspace) => workspace,
        None => {
            incomplete(inventory, "root manifest has no [workspace] table");
            return;
        }
    };
    let members = string_array(inventory, "workspace.members", workspace.get("members"), true);
    let default_members = string_array(
        inventory,
        "workspace.default-members",
        workspace.get("default-members"),
        false,
    );
    let exclude = string_array(
        inventory,
        "workspace.exclude",
        workspace.get("exclude"),
        false,
    );
    for (label, values) in [
        ("workspace.members", &members),
        ("workspace.default-members", &default_members),
        ("workspace.exclude", &exclude),
    ] {
        for (index, value) in values.iter().enumerate() {
            logical(inventory, &format!("{label}:{index}"), value.as_bytes());
        }
    }
    for member in &members {
        let root = discovery_root(&request.workspace_root, member);
        inventory.discovery_roots.insert(root.clone());
        record_discovery_tree(inventory, &root);
    }
    let mut exclude_matchers = Vec::new();
    for pattern in &exclude {
        match globset::Glob::new(pattern) {
            Ok(glob) => exclude_matchers.push((pattern.as_str(), glob.compile_matcher())),
            Err(error) => incomplete(
                inventory,
                format!("invalid workspace exclude glob {pattern}: {error}"),
            ),
        }
    }
    let mut default_matchers = Vec::new();
    for pattern in &default_members {
        match globset::Glob::new(pattern) {
            Ok(glob) => default_matchers.push((pattern.as_str(), glob.compile_matcher())),
            Err(error) => incomplete(
                inventory,
                format!("invalid workspace default-member glob {pattern}: {error}"),
            ),
        }
    }
    for (member_index, member) in members.iter().enumerate() {
        let matcher = match globset::Glob::new(member) {
            Ok(glob) => glob.compile_matcher(),
            Err(error) => {
                incomplete(
                    inventory,
                    format!("invalid workspace member glob {member}: {error}"),
                );
                continue;
            }
        };
        let root = discovery_root(&request.workspace_root, member);
        if member.contains(['*', '?', '[']) {
            for entry in WalkDir::new(&root).follow_links(false).into_iter() {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        incomplete(
                            inventory,
                            format!("workspace candidate discovery failed: {error}"),
                        );
                        continue;
                    }
                };
                if entry.file_name() != OsStr::new("Cargo.toml") {
                    continue;
                }
                let Some(parent) = entry.path().parent() else {
                    incomplete(inventory, "workspace candidate manifest has no parent");
                    continue;
                };
                let Ok(relative) = parent.strip_prefix(&request.workspace_root) else {
                    incomplete(
                        inventory,
                        format!(
                            "workspace candidate is outside the workspace: {}",
                            parent.display()
                        ),
                    );
                    continue;
                };
                let Some(relative) = relative.to_str() else {
                    incomplete(
                        inventory,
                        format!("workspace candidate is not Unicode: {}", relative.display()),
                    );
                    continue;
                };
                let relative = relative.replace('\\', "/");
                let excluded = exclude_matchers
                    .iter()
                    .any(|(_, exclude)| exclude.is_match(&relative));
                let default = default_matchers
                    .iter()
                    .filter(|(_, matcher)| matcher.is_match(&relative))
                    .map(|(pattern, _)| *pattern)
                    .collect::<Vec<_>>()
                    .join("\0");
                logical(
                    inventory,
                    &format!("workspace-candidate:{member_index}:{relative}"),
                    if matcher.is_match(&relative) && !excluded {
                        b"included"
                    } else {
                        b"excluded"
                    },
                );
                logical(
                    inventory,
                    &format!("workspace-candidate-default:{member_index}:{relative}"),
                    default.as_bytes(),
                );
                if matcher.is_match(&relative) && !excluded {
                    inventory.manifests.insert(entry.path().to_path_buf());
                }
            }
        } else {
            let manifest = request.workspace_root.join(member).join("Cargo.toml");
            record_file(
                inventory,
                "workspace-member-manifest",
                &manifest,
                true,
                ExpectedInputKind::File,
            );
            inventory.manifests.insert(manifest);
        }
    }
}

fn record_discovery_tree(inventory: &mut Inventory, root: &Path) {
    record_file(
        inventory,
        "workspace-discovery-root",
        root,
        false,
        ExpectedInputKind::Directory,
    );
    if !root.is_dir() {
        return;
    }
    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        match entry {
            Ok(entry) => {
                if entry.file_type().is_dir() || entry.file_name() == OsStr::new("Cargo.toml") {
                    record_file(
                        inventory,
                        "workspace-candidate",
                        entry.path(),
                        false,
                        ExpectedInputKind::Either,
                    );
                    if entry.file_type().is_dir() {
                        record_file(
                            inventory,
                            "workspace-candidate-manifest-marker",
                            &entry.path().join("Cargo.toml"),
                            false,
                            ExpectedInputKind::File,
                        );
                    }
                }
            }
            Err(error) => incomplete(
                inventory,
                format!(
                    "workspace discovery could not enumerate {}: {error}",
                    root.display()
                ),
            ),
        }
    }
}

fn discover_path_dependencies(inventory: &mut Inventory, manifests: &mut BTreeSet<PathBuf>) {
    let mut pending = manifests.iter().cloned().collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    while let Some(manifest) = pending.pop() {
        if !seen.insert(manifest.clone()) {
            continue;
        }
        let value = match read_toml(&manifest) {
            Ok(value) => value,
            Err(error) => {
                incomplete(inventory, error);
                continue;
            }
        };
        let Some(parent) = manifest.parent() else {
            incomplete(
                inventory,
                format!("manifest has no parent: {}", manifest.display()),
            );
            continue;
        };
        collect_path_dependency_specs(&value, &mut |relative| {
            let dependency_manifest = parent.join(relative).join("Cargo.toml");
            if manifests.insert(dependency_manifest.clone()) {
                pending.push(dependency_manifest);
            }
        });
    }
}

fn collect_path_dependency_specs(value: &toml::Value, callback: &mut impl FnMut(&str)) {
    let Some(table) = value.as_table() else {
        return;
    };
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(dependencies) = table.get(key).and_then(toml::Value::as_table) {
            for spec in dependencies.values() {
                if let Some(path) = spec
                    .as_table()
                    .and_then(|table| table.get("path"))
                    .and_then(toml::Value::as_str)
                {
                    callback(path);
                }
            }
        }
    }
    if let Some(workspace) = table.get("workspace").and_then(toml::Value::as_table) {
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(dependencies) = workspace.get(key).and_then(toml::Value::as_table) {
                collect_paths_from_dependency_table(dependencies, callback);
            }
        }
    }
    if let Some(patches) = table.get("patch").and_then(toml::Value::as_table) {
        for source in patches.values().filter_map(toml::Value::as_table) {
            collect_paths_from_dependency_table(source, callback);
        }
    }
    if let Some(replacements) = table.get("replace").and_then(toml::Value::as_table) {
        collect_paths_from_dependency_table(replacements, callback);
    }
    if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            collect_path_dependency_specs(target, callback);
        }
    }
}

fn collect_paths_from_dependency_table(
    dependencies: &toml::map::Map<String, toml::Value>,
    callback: &mut impl FnMut(&str),
) {
    for spec in dependencies.values() {
        if let Some(path) = spec
            .as_table()
            .and_then(|table| table.get("path"))
            .and_then(toml::Value::as_str)
        {
            callback(path);
        }
    }
}

fn inventory_targets(inventory: &mut Inventory, manifest: &Path) {
    let Some(root) = manifest.parent() else {
        incomplete(
            inventory,
            format!("manifest has no parent: {}", manifest.display()),
        );
        return;
    };
    for relative in ["src/lib.rs", "src/main.rs", "build.rs"] {
        record_file(
            inventory,
            "cargo-target-marker",
            &root.join(relative),
            false,
            ExpectedInputKind::File,
        );
    }
    let value = match read_toml(manifest) {
        Ok(value) => value,
        Err(error) => {
            incomplete(inventory, error);
            return;
        }
    };
    if let Some(package) = value.get("package").and_then(toml::Value::as_table) {
        for key in ["autolib", "autobins", "autoexamples", "autotests", "autobenches"] {
            if let Some(setting) = package.get(key) {
                match setting.as_bool() {
                    Some(setting) => logical(
                        inventory,
                        &format!("cargo-target-setting:{}:{key}", manifest.display()),
                        &[u8::from(setting)],
                    ),
                    None => incomplete(
                        inventory,
                        format!("{key} is not boolean in {}", manifest.display()),
                    ),
                }
            }
        }
        if let Some(build) = package.get("build") {
            match (build.as_str(), build.as_bool()) {
                (Some(path), _) => record_file(
                    inventory,
                    "explicit-build-script",
                    &root.join(path),
                    true,
                    ExpectedInputKind::File,
                ),
                (_, Some(_)) => {}
                _ => incomplete(
                    inventory,
                    format!("package.build has an invalid type in {}", manifest.display()),
                ),
            }
        }
    }
    if let Some(lib) = value.get("lib") {
        inventory_explicit_target(inventory, root, manifest, "lib", lib);
    }
    for key in ["bin", "example", "test", "bench"] {
        let Some(targets) = value.get(key) else {
            continue;
        };
        let Some(targets) = targets.as_array() else {
            incomplete(
                inventory,
                format!("{key} target list is not an array in {}", manifest.display()),
            );
            continue;
        };
        for target in targets {
            inventory_explicit_target(inventory, root, manifest, key, target);
        }
    }
    for directory in ["src/bin", "examples", "tests", "benches"] {
        let directory = root.join(directory);
        record_file(
            inventory,
            "cargo-target-directory",
            &directory,
            false,
            ExpectedInputKind::Directory,
        );
        if directory.is_dir() {
            for entry in WalkDir::new(&directory).follow_links(false).into_iter() {
                match entry {
                    Ok(entry) => {
                        record_file(
                            inventory,
                            "cargo-target-candidate",
                            entry.path(),
                            false,
                            ExpectedInputKind::Either,
                        )
                    }
                    Err(error) => incomplete(
                        inventory,
                        format!(
                            "target discovery failed in {}: {error}",
                            directory.display()
                        ),
                    ),
                }
            }
        }
    }
}

fn inventory_explicit_target(
    inventory: &mut Inventory,
    root: &Path,
    manifest: &Path,
    kind: &str,
    value: &toml::Value,
) {
    let Some(table) = value.as_table() else {
        incomplete(
            inventory,
            format!("{kind} target is not a table in {}", manifest.display()),
        );
        return;
    };
    let Some(path) = table.get("path") else {
        return;
    };
    let Some(path) = path.as_str() else {
        incomplete(
            inventory,
            format!("{kind} target path is not a string in {}", manifest.display()),
        );
        return;
    };
    record_file(
        inventory,
        "explicit-cargo-target",
        &root.join(path),
        true,
        ExpectedInputKind::File,
    );
}

fn validate_lockfile(inventory: &mut Inventory, lockfile: &Path) {
    let value = match read_toml(lockfile) {
        Ok(value) => value,
        Err(error) => {
            incomplete(inventory, error);
            return;
        }
    };
    let Some(packages) = value.get("package").and_then(toml::Value::as_array) else {
        incomplete(inventory, "Cargo.lock has no package inventory");
        return;
    };
    for package in packages {
        let Some(table) = package.as_table() else {
            incomplete(inventory, "Cargo.lock contains a malformed package record");
            continue;
        };
        let source = table.get("source").and_then(toml::Value::as_str);
        match source {
            Some(source) if source.starts_with("registry+") => {
                if table
                    .get("checksum")
                    .and_then(toml::Value::as_str)
                    .is_none()
                {
                    incomplete(
                        inventory,
                        format!(
                            "registry dependency is not checksum-locked: {}",
                            table
                                .get("name")
                                .and_then(toml::Value::as_str)
                                .unwrap_or("unknown")
                        ),
                    );
                }
            }
            Some(source) if source.starts_with("git+") => {
                let revision = source.rsplit_once('#').map(|(_, revision)| revision);
                if !revision.is_some_and(|revision| {
                    matches!(revision.len(), 40 | 64)
                        && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
                }) {
                    incomplete(
                        inventory,
                        format!("Git dependency is not revision-locked: {source}"),
                    );
                }
            }
            Some(source) if !source.starts_with("path+") => {
                incomplete(
                    inventory,
                    format!("dependency source cannot be enumerated: {source}"),
                );
            }
            _ => {}
        }
    }
}

fn run_metadata(request: &CargoMetadataRequest, locked: bool) -> Result<Value, CargoCacheError> {
    let args = metadata_args(request, locked);
    let output = Command::new(&request.cargo)
        .args(&args)
        .current_dir(&request.workspace_root)
        .env("RUSTC", &request.rustc)
        .output()
        .map_err(|error| CargoCacheError::Metadata(error.to_string()))?;
    if !output.status.success() {
        return Err(CargoCacheError::Metadata(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn metadata_args(request: &CargoMetadataRequest, locked: bool) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("metadata"),
        OsString::from("--format-version"),
        OsString::from("1"),
    ];
    if locked {
        args.push(OsString::from("--locked"));
    }
    args.extend(request.extra_args.clone());
    args
}

fn metadata_manifest_paths(metadata: &Value) -> Result<BTreeSet<PathBuf>, String> {
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| "Cargo metadata has no package array".to_string())?;
    let mut manifests = BTreeSet::new();
    for (index, package) in packages.iter().enumerate() {
        let package = package
            .as_object()
            .ok_or_else(|| format!("Cargo metadata package {index} is not an object"))?;
        let manifest = package
            .get("manifest_path")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Cargo metadata package {index} has no manifest_path"))?;
        let manifest = fs::canonicalize(manifest)
            .map_err(|error| format!("Cargo metadata manifest cannot be canonicalized: {error}"))?;
        if !manifests.insert(manifest) {
            return Err(format!(
                "Cargo metadata repeats manifest_path for package {index}"
            ));
        }
    }
    Ok(manifests)
}

fn invocation_seed(request: &CargoMetadataRequest) -> String {
    let mut writer = FingerprintWriter::default();
    writer.field(b"cache-format", &CACHE_FORMAT_VERSION.to_be_bytes());
    writer.field(
        b"repository-root",
        &native_path_bytes(&request.repository_root),
    );
    writer.field(
        b"workspace-root",
        &native_path_bytes(&request.workspace_root),
    );
    writer.field(b"environment-mode", request.environment_mode.as_bytes());
    writer.field(b"cargo", &native_os_bytes(&request.cargo));
    writer.field(b"rustc", &native_os_bytes(&request.rustc));
    for argument in metadata_args(request, true) {
        writer.field(b"arg", &native_os_bytes(&argument));
    }
    writer.finish_hex()
}

fn complete_fingerprint(request: &CargoMetadataRequest, inventory: &Inventory) -> String {
    let mut writer = FingerprintWriter::default();
    writer.field(b"seed", invocation_seed(request).as_bytes());
    for (label, value) in &inventory.logical {
        writer.field(label.as_bytes(), value);
    }
    for (path, record) in &inventory.files {
        writer.field(b"path", path);
        writer.field(b"label", record.label.as_bytes());
        writer.field(b"type", record.file_type.as_bytes());
        writer.field(b"present", &[u8::from(record.present)]);
        writer.field(b"length", &record.length.to_be_bytes());
        writer.field(
            b"digest",
            record.sha256.as_deref().unwrap_or_default().as_bytes(),
        );
    }
    writer.field(b"complete", &[u8::from(inventory.complete)]);
    for reason in &inventory.reasons {
        writer.field(b"reason", reason.as_bytes());
    }
    format!("sha256:{}", writer.finish_hex())
}

#[derive(Default)]
struct FingerprintWriter {
    bytes: Vec<u8>,
}

impl FingerprintWriter {
    fn field(&mut self, label: &[u8], value: &[u8]) {
        self.bytes
            .extend_from_slice(&u64::try_from(label.len()).unwrap_or(u64::MAX).to_be_bytes());
        self.bytes.extend_from_slice(label);
        self.bytes
            .extend_from_slice(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        self.bytes.extend_from_slice(value);
    }

    fn finish_hex(self) -> String {
        format!("{:x}", Sha256::digest(self.bytes))
    }
}

fn record_file(
    inventory: &mut Inventory,
    label: &str,
    path: &Path,
    required: bool,
    expected: ExpectedInputKind,
) {
    let path_bytes = native_path_bytes(path);
    if let Some(existing) = inventory.files.get(&path_bytes) {
        let existing = existing.clone();
        validate_input_record(inventory, path, required, expected, &existing);
        return;
    }
    let metadata = fs::symlink_metadata(path);
    let mut record = InputRecord {
        label: label.to_string(),
        path_bytes_base64: base64_encode(&path_bytes),
        file_type: "absent".to_string(),
        present: false,
        length: 0,
        sha256: None,
    };
    match metadata {
        Ok(metadata) => {
            record.present = true;
            record.length = metadata.len();
            if metadata.file_type().is_symlink() {
                record.file_type = "symlink".to_string();
                incomplete(
                    inventory,
                    format!(
                        "symlinked fingerprint input is ambiguous: {}",
                        path.display()
                    ),
                );
            } else if metadata.is_file() {
                record.file_type = "file".to_string();
                match fs::read(path) {
                    Ok(bytes) => record.sha256 = Some(format!("{:x}", Sha256::digest(bytes))),
                    Err(error) => incomplete(
                        inventory,
                        format!(
                            "fingerprint input is unreadable ({}): {error}",
                            path.display()
                        ),
                    ),
                }
            } else if metadata.is_dir() {
                record.file_type = "directory".to_string();
            } else {
                record.file_type = "other".to_string();
                incomplete(
                    inventory,
                    format!(
                        "fingerprint input is not a regular file/directory: {}",
                        path.display()
                    ),
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if required {
                incomplete(
                    inventory,
                    format!("required fingerprint input is absent: {}", path.display()),
                );
            }
        }
        Err(error) => incomplete(
            inventory,
            format!(
                "fingerprint input cannot be inspected ({}): {error}",
                path.display()
            ),
        ),
    }
    validate_input_record(inventory, path, required, expected, &record);
    inventory.files.insert(path_bytes, record);
}

fn validate_input_record(
    inventory: &mut Inventory,
    path: &Path,
    required: bool,
    expected: ExpectedInputKind,
    record: &InputRecord,
) {
    if required && !record.present {
        incomplete(
            inventory,
            format!("required fingerprint input is absent: {}", path.display()),
        );
        return;
    }
    if !record.present {
        return;
    }
    let matches = match expected {
        ExpectedInputKind::File => record.file_type == "file",
        ExpectedInputKind::Directory => record.file_type == "directory",
        ExpectedInputKind::Either => matches!(record.file_type.as_str(), "file" | "directory"),
    };
    if !matches {
        incomplete(
            inventory,
            format!(
                "fingerprint input has the wrong file type (expected {expected:?}, found {}): {}",
                record.file_type,
                path.display()
            ),
        );
    }
}

fn record_command_version(
    inventory: &mut Inventory,
    label: &str,
    program: &OsStr,
    args: &[OsString],
    cwd: &Path,
) {
    match Command::new(program).args(args).current_dir(cwd).output() {
        Ok(output) if output.status.success() => logical(inventory, label, &output.stdout),
        Ok(output) => incomplete(
            inventory,
            format!(
                "{label} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ),
        Err(error) => incomplete(inventory, format!("{label} could not run: {error}")),
    }
}

fn record_git_identity(inventory: &mut Inventory, repository_root: &Path) {
    for (label, args) in [
        ("git-root", ["rev-parse", "--show-toplevel"].as_slice()),
        ("git-dir", ["rev-parse", "--absolute-git-dir"].as_slice()),
        (
            "git-common-dir",
            ["rev-parse", "--git-common-dir"].as_slice(),
        ),
    ] {
        match Command::new("git")
            .args(args)
            .current_dir(repository_root)
            .env("LC_ALL", "C")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
        {
            Ok(output) if output.status.success() => {
                match canonical_git_output_path(repository_root, &output.stdout) {
                    Ok(path) => logical_path(inventory, label, &path),
                    Err(error) => incomplete(inventory, format!("{label} is invalid: {error}")),
                }
            }
            Ok(output) => incomplete(
                inventory,
                format!(
                    "{label} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ),
            Err(error) => incomplete(inventory, format!("{label} could not run: {error}")),
        }
    }
    logical_path(inventory, "worktree-identity", repository_root);
}

fn canonical_git_output_path(repository_root: &Path, output: &[u8]) -> Result<PathBuf, String> {
    let output = output.strip_suffix(b"\n").unwrap_or(output);
    let output = output.strip_suffix(b"\r").unwrap_or(output);
    if output.is_empty() || output.contains(&b'\n') || output.contains(&b'\r') || output.contains(&0)
    {
        return Err("expected exactly one path line".to_string());
    }
    let path = git_output_path(output)?;
    let path = if path.is_absolute() {
        path
    } else {
        repository_root.join(path)
    };
    fs::canonicalize(&path)
        .map_err(|error| format!("failed to canonicalize {}: {error}", path.display()))
}

#[cfg(unix)]
fn git_output_path(output: &[u8]) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(output.to_vec())))
}

#[cfg(windows)]
fn git_output_path(output: &[u8]) -> Result<PathBuf, String> {
    std::str::from_utf8(output)
        .map(PathBuf::from)
        .map_err(|_| "Git path output is not UTF-8".to_string())
}

#[cfg(not(any(unix, windows)))]
fn git_output_path(output: &[u8]) -> Result<PathBuf, String> {
    std::str::from_utf8(output)
        .map(PathBuf::from)
        .map_err(|_| "Git path output is not UTF-8".to_string())
}

fn read_candidate_entry(
    cache_root: &Path,
    index_path: &Path,
) -> Result<Option<CacheEntry>, CargoCacheError> {
    let index_bytes = match fs::read(index_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CargoCacheError::Io {
                path: index_path.to_path_buf(),
                source,
            });
        }
    };
    let index: CacheIndex = match serde_json::from_slice::<CacheIndex>(&index_bytes) {
        Ok(index) if index.cache_format_version == CACHE_FORMAT_VERSION => index,
        _ => return Ok(None),
    };
    let hex = index
        .fingerprint
        .strip_prefix("sha256:")
        .unwrap_or(&index.fingerprint);
    let entry_path = cache_root.join("entries").join(format!("{hex}.json"));
    let bytes = match fs::read(&entry_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CargoCacheError::Io {
                path: entry_path,
                source,
            });
        }
    };
    let entry: CacheEntry = match serde_json::from_slice::<CacheEntry>(&bytes) {
        Ok(entry) if entry.cache_format_version == CACHE_FORMAT_VERSION => entry,
        _ => return Ok(None),
    };
    Ok(Some(entry))
}

fn store_entry(
    cache_root: &Path,
    index_path: &Path,
    entry: &CacheEntry,
) -> Result<(), CargoCacheError> {
    let entries = cache_root.join("entries");
    let indexes = cache_root.join("index");
    fs::create_dir_all(&entries).map_err(|source| CargoCacheError::Io {
        path: entries.clone(),
        source,
    })?;
    fs::create_dir_all(&indexes).map_err(|source| CargoCacheError::Io {
        path: indexes.clone(),
        source,
    })?;
    let hex = entry
        .fingerprint
        .strip_prefix("sha256:")
        .unwrap_or(&entry.fingerprint);
    let entry_path = entries.join(format!("{hex}.json"));
    if !entry_path.exists() {
        atomic_write_noclobber(&entry_path, &serde_json::to_vec(entry)?)?;
    }
    let index = CacheIndex {
        cache_format_version: CACHE_FORMAT_VERSION,
        fingerprint: entry.fingerprint.clone(),
    };
    atomic_replace(index_path, &serde_json::to_vec(&index)?)
}

fn atomic_write_noclobber(path: &Path, bytes: &[u8]) -> Result<(), CargoCacheError> {
    let parent = path.parent().expect("cache path has parent");
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| CargoCacheError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    temporary
        .write_all(bytes)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|source| CargoCacheError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    match temporary.persist_noclobber(path) {
        Ok(_) => sync_directory(parent),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(CargoCacheError::Io {
            path: path.to_path_buf(),
            source: error.error,
        }),
    }
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), CargoCacheError> {
    let parent = path.parent().expect("cache path has parent");
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| CargoCacheError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    temporary
        .write_all(bytes)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|source| CargoCacheError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| CargoCacheError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    sync_directory(parent)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), CargoCacheError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| CargoCacheError::Io {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> Result<(), CargoCacheError> {
    Ok(())
}

fn read_toml(path: &Path) -> Result<toml::Value, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    toml::from_str(&text).map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

fn string_array(
    inventory: &mut Inventory,
    label: &str,
    value: Option<&toml::Value>,
    required: bool,
) -> Vec<String> {
    let Some(value) = value else {
        logical(inventory, &format!("{label}:present"), b"0");
        if required {
            incomplete(inventory, format!("{label} is absent"));
        }
        return Vec::new();
    };
    logical(inventory, &format!("{label}:present"), b"1");
    let Some(values) = value.as_array() else {
        incomplete(inventory, format!("{label} is not an array"));
        return Vec::new();
    };
    let mut result = Vec::new();
    for (index, value) in values.iter().enumerate() {
        match value.as_str() {
            Some(value) => result.push(value.to_string()),
            None => incomplete(
                inventory,
                format!("{label}[{index}] is not a string"),
            ),
        }
    }
    result
}

fn discovery_root(workspace_root: &Path, pattern: &str) -> PathBuf {
    let prefix = pattern
        .split('/')
        .take_while(|component| !component.contains(['*', '?', '[']))
        .collect::<PathBuf>();
    workspace_root.join(prefix)
}

fn relevant_environment() -> Vec<(OsString, OsString)> {
    let mut values = std::env::vars_os()
        .filter(|(key, _)| is_relevant_environment_key(key))
        .collect::<Vec<_>>();
    values.sort_by(|(left, _), (right, _)| native_os_bytes(left).cmp(&native_os_bytes(right)));
    values
}

fn effective_cargo_home(workspace_root: &Path) -> Option<PathBuf> {
    resolve_cargo_home(
        workspace_root,
        std::env::var_os("CARGO_HOME"),
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }),
    )
}

fn resolve_cargo_home(
    workspace_root: &Path,
    configured: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    let path = configured
        .map(PathBuf::from)
        .or_else(|| home.map(PathBuf::from).map(|home| home.join(".cargo")))?;
    Some(if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    })
}

fn is_relevant_environment_key(key: &OsStr) -> bool {
    key.to_str().is_none_or(|key| {
        key.starts_with("CARGO_")
            || key.starts_with("RUST_")
            || key.starts_with("RUSTC_")
            || key.starts_with("RUSTUP_")
            || matches!(key, "PATH" | "RUSTC" | "RUSTDOC" | "RUSTFLAGS" | "RUSTDOCFLAGS")
    })
}

fn validate_metadata_topology_args(inventory: &mut Inventory, arguments: &[OsString]) {
    for argument in arguments {
        let Some(argument) = argument.to_str() else {
            incomplete(inventory, "metadata argument is not Unicode and cannot be modeled");
            continue;
        };
        if matches!(argument, "--manifest-path" | "--config")
            || argument.starts_with("--manifest-path=")
            || argument.starts_with("--config=")
        {
            incomplete(
                inventory,
                format!("metadata topology argument is not cache-enumerable: {argument}"),
            );
        }
    }
}

fn resolve_executable(program: &OsStr) -> Option<PathBuf> {
    let path = PathBuf::from(program);
    if path.components().count() > 1 {
        return fs::canonicalize(path).ok();
    }
    let path_env = std::env::var_os("PATH")?;
    #[cfg(windows)]
    let extensions = std::env::var_os("PATHEXT")
        .unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"))
        .to_string_lossy()
        .split(';')
        .map(str::to_string)
        .collect::<Vec<_>>();
    for directory in std::env::split_paths(&path_env) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return fs::canonicalize(candidate).ok();
        }
        #[cfg(windows)]
        for extension in &extensions {
            let candidate = directory.join(format!("{}{}", program.to_string_lossy(), extension));
            if candidate.is_file() {
                return fs::canonicalize(candidate).ok();
            }
        }
    }
    None
}

fn logical(inventory: &mut Inventory, label: &str, value: impl AsRef<[u8]>) {
    inventory
        .logical
        .insert(label.to_string(), value.as_ref().to_vec());
}

fn logical_os(inventory: &mut Inventory, label: &str, value: &OsStr) {
    logical(inventory, label, native_os_bytes(value));
}

fn logical_path(inventory: &mut Inventory, label: &str, value: &Path) {
    logical(inventory, label, native_path_bytes(value));
}

fn incomplete(inventory: &mut Inventory, reason: impl Into<String>) {
    inventory.complete = false;
    inventory.reasons.insert(reason.into());
}

fn canonical(path: &Path) -> Result<PathBuf, CargoCacheError> {
    fs::canonicalize(path).map_err(|source| CargoCacheError::Canonicalize {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn native_os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn native_os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>()
}

fn native_path_bytes(path: &Path) -> Vec<u8> {
    native_os_bytes(path.as_os_str())
}

#[cfg(unix)]
fn native_path_from_bytes(bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(windows)]
fn native_path_from_bytes(bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let wide = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    Some(PathBuf::from(OsString::from_wide(&wide)))
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
#[path = "cargo_cache_tests.rs"]
mod tests;
