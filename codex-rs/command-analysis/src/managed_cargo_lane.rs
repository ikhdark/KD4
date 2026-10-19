//! Containment for recognizable raw Cargo launches, not an OS sandbox or a
//! claim about Cargo freshness. Managed Python/Just children are not tool calls.
use std::collections::HashMap;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use super::argv_commands;
use super::infer_direct_shell_type;
use super::program_name;
use codex_shell_command::shell_detect::ShellType;

fn rejected(detail: &str) -> String {
    format!(
        "Raw Cargo launch rejected; no build started. {detail} Cargo admission resolves the command's effective target/build directories; ordinary Rust workspaces do not require a repository-specific runner."
    )
}

/// Build rejection whose remediation matches the tree the command ran in.
/// Inside a tree that publishes a lane marker, the neutral "no repository-specific
/// runner" advice contradicts that tree's own managed entrypoint, so name the
/// marker instead and let the caller consult its documented runner.
fn rejected_build(detail: &str, cwd: &Path) -> String {
    match managed_target(cwd) {
        Ok(Some(root)) => format!(
            "Raw Cargo launch rejected; no build started. {detail} Cargo admission resolves the command's effective target/build directories; this tree is reserved by a lane marker under {}, so use the managed test/build entrypoint documented for it rather than a raw Cargo command.",
            root.display()
        ),
        _ => rejected(detail),
    }
}

pub async fn check_managed_cargo_lane(
    command: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Result<(), String> {
    // Ordinary inspection and managed entrypoints need no filesystem work or
    // blocking worker. This boundary recognizes Cargo, not arbitrary programs
    // that might secretly spawn it.
    if !command
        .iter()
        .any(|arg| arg.to_ascii_lowercase().contains("cargo"))
    {
        return Ok(());
    }
    let (command, cwd, env) = (command.to_vec(), cwd.to_path_buf(), env.clone());
    crate::run_blocking_command_analysis(move || check(&command, &cwd, &env, 0))
        .await
        .map_err(|error| rejected(&format!("Target admission worker failed: {error}")))?
}

fn check(
    command: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    depth: usize,
) -> Result<(), String> {
    if depth > 8 {
        return Err(rejected(
            "Too many indirect command wrappers to resolve the target.",
        ));
    }
    let Some((program, args)) = command.split_first() else {
        return Ok(());
    };
    if matches!(
        program_name(program).to_ascii_lowercase().as_str(),
        "start-process" | "invoke-expression" | "iex" | "start"
    ) && visibly_builds(&args.join(" "))
    {
        return Err(rejected(
            "Cannot resolve Cargo's target through a process-launching shell builtin; use standalone argv.",
        ));
    }
    if let Some(shell) = infer_direct_shell_type(command) {
        let parsed = argv_commands(command, Some(shell));
        // A flattened command list cannot model assignments, cd, functions or
        // expansion. Only a standalone literal command proves its environment.
        let standalone = match shell {
            ShellType::PowerShell => codex_shell_command::powershell::extract_powershell_command(
                command,
            )
            .and_then(|(_, script)| {
                codex_shell_command::validation::standalone_powershell_argv(script)
            }),
            ShellType::Bash | ShellType::Sh | ShellType::Zsh => command
                .last()
                .and_then(|script| codex_shell_command::validation::standalone_argv(script)),
            ShellType::Cmd => parsed
                .as_ref()
                .filter(|stages| stages.len() == 1)
                .and_then(|stages| stages.first().cloned())
                .filter(|stage| !stage.iter().any(|arg| arg.contains(['%', '!']))),
        };
        if let Some(argv) = standalone {
            return check(&argv, cwd, env, depth + 1);
        }
        // Inspect parsed leaves so read-only Cargo/ordinary text searches stay
        // usable. Reject opaque Cargo-bearing scripts rather than guessing the
        // effective directory after shell-local environment/cwd changes.
        let inspection_only = parsed.as_ref().is_some_and(|stages| {
            !stages.is_empty()
                && stages.iter().all(|stage| {
                    stage.first().is_some_and(|program| {
                        read_only_inspection_program(program)
                            || (program_name(program).eq_ignore_ascii_case("cargo")
                                && !potential_build(stage))
                    })
                })
        });
        // A trustworthy parse already carries clause boundaries, and each stage
        // is checked below. Scanning the flattened script text in that case only
        // reintroduces false positives from Cargo names inside string literals
        // and argument lists, so keep the text fallback for opaque scripts.
        if let Some(offending) = parsed
            .as_ref()
            .and_then(|stages| stages.iter().find(|stage| potential_build(stage)))
        {
            return Err(rejected_build(
                &format!(
                    "Cannot resolve a raw Cargo build's effective target through this script; the `{}` clause would build. Use a standalone Cargo command with an explicit workdir so configuration and directory ownership can be resolved.",
                    offending.join(" ")
                ),
                cwd,
            ));
        }
        if parsed.is_none() && !inspection_only && args.iter().any(|script| visibly_builds(script))
        {
            return Err(rejected_build(
                "Cannot resolve a raw Cargo build's effective target through this script, which names a Cargo build token but could not be parsed into separate commands. Reissue it as standalone argv, or as a command whose program is recognizably read-only, so the effective target can be resolved.",
                cwd,
            ));
        }
        if let Some(stages) = parsed {
            for stage in stages {
                check(&stage, cwd, env, depth + 1)?;
            }
        }
        return Ok(());
    }
    if program_name(program).eq_ignore_ascii_case("env") {
        let mut child = env.clone();
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            if matches!(arg.as_str(), "-i" | "--ignore-environment") {
                child.clear();
                index += 1;
                continue;
            }
            if matches!(arg.as_str(), "-u" | "--unset") || arg.starts_with("--unset=") {
                let key = if let Some((_, key)) = arg.split_once('=') {
                    key
                } else {
                    index += 1;
                    args.get(index)
                        .map(String::as_str)
                        .ok_or_else(|| rejected("env --unset has no name."))?
                };
                child.retain(|name, _| {
                    if cfg!(windows) {
                        !name.eq_ignore_ascii_case(key)
                    } else {
                        name != key
                    }
                });
                index += 1;
                continue;
            }
            if arg == "--" {
                index += 1;
                break;
            }
            if let Some((key, value)) = arg.split_once('=') {
                if cfg!(windows) {
                    child.retain(|name, _| !name.eq_ignore_ascii_case(key));
                }
                child.insert(key.to_string(), value.to_string());
                index += 1;
            } else if arg.starts_with('-') {
                return Err(rejected("Unsupported env wrapper; use literal Cargo argv."));
            } else {
                break;
            }
        }
        return check(&args[index..], cwd, &child, depth + 1);
    }
    if matches!(
        program_name(program).to_ascii_lowercase().as_str(),
        "command" | "exec" | "call"
    ) {
        return check(
            args.strip_prefix(&["--".to_string()]).unwrap_or(args),
            cwd,
            env,
            depth + 1,
        );
    }
    if program_name(program).eq_ignore_ascii_case("rustup")
        && args.first().is_some_and(|arg| arg == "run")
    {
        return check(args.get(2..).unwrap_or_default(), cwd, env, depth + 1);
    }
    if !program_name(program).eq_ignore_ascii_case("cargo") {
        return Ok(());
    }
    let Some((subcommand_index, subcommand)) = cargo_subcommand(args) else {
        return Ok(());
    };
    if args[..subcommand_index]
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--version" | "-V"))
    {
        return Ok(());
    }
    if cargo_builtin(subcommand) && !raw_build(command) {
        return Ok(());
    }
    let original_cwd = cwd;
    let mut cwd = cwd.to_path_buf();
    let mut target = None;
    let mut overrides = Vec::new();
    let mut manifest = None;
    let mut tokens = args.iter().take_while(|arg| arg.as_str() != "--");
    while let Some(arg) = tokens.next() {
        let (key, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
        if matches!(key, "--target-dir" | "--config" | "--manifest-path" | "-C") {
            let value = inline
                .or_else(|| tokens.next().map(String::as_str))
                .ok_or_else(|| rejected("A target-affecting Cargo option has no value."))?;
            match key {
                "--target-dir" => target = Some(value.to_string()),
                "--config" => overrides.push(value.to_string()),
                "--manifest-path" => manifest = Some(value.to_string()),
                "-C" => cwd = cwd.join(value),
                _ => unreachable!(),
            }
        } else if let Some(value) = arg.strip_prefix("-C").filter(|v| !v.is_empty()) {
            cwd = cwd.join(value);
        }
    }
    // Even an explicit final-artifact directory does not override build-dir.
    // Unknown plugins use the same directory admission, not a name allowlist.
    let home = cargo_home(&cwd, env);
    let mut budget = ConfigReadBudget::default();
    let mut configured = CargoDirectories::default();
    let mut dirs = home.iter().cloned().collect::<Vec<_>>();
    dirs.extend(
        cwd.ancestors()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|p| p.join(".cargo")),
    );
    for dir in dirs {
        // Cargo prefers the legacy filename if both forms exist.
        let legacy = dir.join("config");
        let path = if config_exists(&legacy)? {
            legacy
        } else {
            dir.join("config.toml")
        };
        if config_exists(&path)? {
            configured.merge(read_config(&path, &mut budget, &mut Vec::new())?);
        }
    }
    // Cargo expands file/environment aliases before applying CLI --config.
    // Metadata probes also confirm that aliases can shadow external plugins.
    if !cargo_builtin(subcommand) {
        let environment_key = format!(
            "CARGO_ALIAS_{}",
            subcommand.to_ascii_uppercase().replace('-', "_")
        );
        let alias = env_value(env, &environment_key)
            .map(|value| toml::Value::String(value.to_string()))
            .or_else(|| configured.aliases.remove(subcommand));
        if let Some(alias) = alias {
            let words = match alias {
                // Cargo strings split on whitespace, not shell quoting. Arrays
                // are required to preserve a single argument containing spaces.
                toml::Value::String(value) => value
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
                toml::Value::Array(values) => values
                    .into_iter()
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::to_string)
                            .ok_or_else(|| rejected("Cargo alias arguments must be strings."))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return Err(rejected("Cargo alias must be a string or string array.")),
            };
            if words.is_empty() {
                return Err(rejected("Cargo alias is empty."));
            }
            let mut expanded = command[..subcommand_index + 1].to_vec();
            expanded.extend(words);
            expanded.extend_from_slice(&args[subcommand_index + 1..]);
            return check(&expanded, original_cwd, env, depth + 1);
        }
    }
    if !raw_build(command) {
        return Ok(());
    }
    for (key, destination) in [
        ("CARGO_BUILD_TARGET_DIR", &mut configured.target),
        ("CARGO_BUILD_BUILD_DIR", &mut configured.build),
    ] {
        if let Some(value) = env_value(env, key) {
            *destination = Some(ConfigDirectory::new(value, &cwd));
        }
    }
    for value in overrides {
        let layer = if value.contains('=') {
            let value = toml::from_str::<toml::Value>(&value)
                .map_err(|_| rejected("Cannot parse Cargo --config override."))?;
            config_values(&value, &cwd, &cwd, &mut budget, &mut Vec::new())?
        } else {
            read_config(&cwd.join(value), &mut budget, &mut Vec::new())?
        };
        configured.merge(layer);
    }
    // Independently verified with Cargo 1.98.1 metadata: the dedicated variable
    // beats --config build.target-dir, unlike CARGO_BUILD_TARGET_DIR.
    if let Some(value) = env_value(env, "CARGO_TARGET_DIR") {
        configured.target = Some(ConfigDirectory::new(value, &cwd));
    }
    if let Some(value) = target {
        configured.target = Some(ConfigDirectory::new(&value, &cwd));
    }
    let needs_workspace = configured.target.is_none()
        || configured
            .build
            .as_ref()
            .is_some_and(|value| value.value.contains("{workspace-root}"));
    let workspace = if needs_workspace {
        workspace_root(&cwd, manifest.as_deref(), &mut budget)?
    } else {
        cwd.clone()
    };
    let target = configured
        .target
        .map(|value| value.base.join(value.value))
        .unwrap_or_else(|| workspace.join("target"));
    let build = match configured.build {
        Some(value) => value.build_path(&workspace, home.as_deref())?,
        None => target.clone(),
    };
    for (kind, path) in [("target", target), ("build", build)] {
        if let Some(managed) = managed_target(&path)? {
            return Err(rejected(&format!(
                "Effective {kind} directory: {}. It belongs to managed Cargo directory {}. Use that workspace's documented managed build/test entrypoint to reserve it before launching Cargo.",
                path.display(),
                managed.display(),
            )));
        }
    }
    Ok(())
}

fn env_value<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.iter()
        .find(|(name, _)| {
            if cfg!(windows) {
                name.eq_ignore_ascii_case(key)
            } else {
                name.as_str() == key
            }
        })
        .map(|(_, value)| value.as_str())
}

#[derive(Default)]
struct CargoDirectories {
    target: Option<ConfigDirectory>,
    build: Option<ConfigDirectory>,
    aliases: HashMap<String, toml::Value>,
}

impl CargoDirectories {
    fn merge(&mut self, layer: Self) {
        if layer.target.is_some() {
            self.target = layer.target;
        }
        if layer.build.is_some() {
            self.build = layer.build;
        }
        for (name, value) in layer.aliases {
            match (self.aliases.get_mut(&name), value) {
                (Some(toml::Value::Array(previous)), toml::Value::Array(later)) => {
                    previous.extend(later)
                }
                (_, value) => {
                    self.aliases.insert(name, value);
                }
            }
        }
    }
}

struct ConfigDirectory {
    value: String,
    base: PathBuf,
}

impl ConfigDirectory {
    fn new(value: &str, base: &Path) -> Self {
        Self {
            value: value.to_string(),
            base: base.to_path_buf(),
        }
    }

    fn build_path(self, workspace: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
        let mut value = self
            .value
            .replace("{workspace-root}", &workspace.to_string_lossy());
        if value.contains("{cargo-cache-home}") {
            let home =
                home.ok_or_else(|| rejected("Cannot resolve Cargo home in build.build-dir."))?;
            value = value.replace("{cargo-cache-home}", &home.to_string_lossy());
        }
        // Do not invent Cargo's workspace-path hash or miss a junction at the
        // real expanded path. A literal override can resolve this uncertainty.
        if value.contains(['{', '}']) {
            return Err(rejected(
                "Cannot resolve a build.build-dir template; use a literal directory.",
            ));
        }
        Ok(self.base.join(value))
    }
}

fn cargo_home(cwd: &Path, env: &HashMap<String, String>) -> Option<PathBuf> {
    env_value(env, "CARGO_HOME")
        .map(|path| cwd.join(path))
        .or_else(|| {
            env_value(env, if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|home| Path::new(home).join(".cargo"))
        })
}

#[derive(Default)]
struct ConfigReadBudget {
    files: usize,
    bytes: usize,
}

fn config_exists(path: &Path) -> Result<bool, String> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(rejected("Cannot inspect Cargo configuration.")),
    }
}

fn read_toml(path: &Path, budget: &mut ConfigReadBudget) -> Result<toml::Value, String> {
    const MAX_BYTES: usize = 1024 * 1024;
    if budget.files >= 64 || budget.bytes >= MAX_BYTES {
        return Err(rejected(
            "Cargo configuration exceeds the bounded admission read.",
        ));
    }
    budget.files += 1;
    let file =
        std::fs::File::open(path).map_err(|_| rejected("Cannot read Cargo configuration."))?;
    let mut text = String::new();
    file.take((MAX_BYTES - budget.bytes + 1) as u64)
        .read_to_string(&mut text)
        .map_err(|_| rejected("Cannot read Cargo configuration."))?;
    budget.bytes += text.len();
    if budget.bytes > MAX_BYTES {
        return Err(rejected(
            "Cargo configuration exceeds the bounded admission read.",
        ));
    }
    // Value::from_str parses a single TOML value, not a Cargo document. Even
    // ordinary [env], [package], and [workspace] tables require from_str.
    toml::from_str(&text).map_err(|_| {
        rejected(&format!(
            "Cannot parse Cargo configuration {} to resolve its directories.",
            path.display()
        ))
    })
}

fn read_config(
    path: &Path,
    budget: &mut ConfigReadBudget,
    active: &mut Vec<PathBuf>,
) -> Result<CargoDirectories, String> {
    let identity = dunce::canonicalize(path)
        .map_err(|_| rejected("Cannot read an included Cargo configuration."))?;
    if active.len() >= 16 || active.contains(&identity) {
        return Err(rejected(
            "Cargo configuration include cycle or depth limit.",
        ));
    }
    active.push(identity);
    let result = (|| {
        let config = read_toml(path, budget)?;
        let directory = path
            .parent()
            .ok_or_else(|| rejected("Cargo config has no parent directory."))?;
        // Includes resolve beside their including file, but directory values
        // use their own file's parent-of-parent. Confirmed by metadata probes.
        config_values(
            &config,
            directory.parent().unwrap_or(directory),
            directory,
            budget,
            active,
        )
    })();
    active.pop();
    result
}

fn config_values(
    config: &toml::Value,
    base: &Path,
    include_dir: &Path,
    budget: &mut ConfigReadBudget,
    active: &mut Vec<PathBuf>,
) -> Result<CargoDirectories, String> {
    let mut result = CargoDirectories::default();
    if let Some(include) = config.get("include") {
        let includes = include
            .as_array()
            .ok_or_else(|| rejected("Cargo include must be an array."))?;
        for item in includes {
            let (path, optional) = if let Some(path) = item.as_str() {
                (path, false)
            } else {
                let path = item
                    .get("path")
                    .and_then(toml::Value::as_str)
                    .ok_or_else(|| rejected("Cargo include is missing its path."))?;
                let optional = item
                    .get("optional")
                    .map(|value| {
                        value
                            .as_bool()
                            .ok_or_else(|| rejected("Cargo include.optional must be boolean."))
                    })
                    .transpose()?
                    .unwrap_or(false);
                (path, optional)
            };
            let path = include_dir.join(path);
            if optional && !config_exists(&path)? {
                continue;
            }
            result.merge(read_config(&path, budget, active)?);
        }
    }
    // Later includes override earlier includes; the including file wins last.
    if let Some(aliases) = config.get("alias").and_then(toml::Value::as_table) {
        result.merge(CargoDirectories {
            aliases: aliases
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            ..Default::default()
        });
    }
    for (key, destination) in [
        ("target-dir", &mut result.target),
        ("build-dir", &mut result.build),
    ] {
        if let Some(value) = config.get("build").and_then(|build| build.get(key)) {
            let value = value
                .as_str()
                .ok_or_else(|| rejected(&format!("Cargo build.{key} is not a string.")))?;
            *destination = Some(ConfigDirectory::new(value, base));
        }
    }
    Ok(result)
}

fn workspace_root(
    cwd: &Path,
    manifest: Option<&str>,
    budget: &mut ConfigReadBudget,
) -> Result<PathBuf, String> {
    let manifest = manifest.map(|path| cwd.join(path)).or_else(|| {
        cwd.ancestors()
            .map(|directory| directory.join("Cargo.toml"))
            .find(|path| path.is_file())
    });
    let Some(manifest) = manifest else {
        return Ok(cwd.to_path_buf());
    };
    let package = read_toml(&manifest, budget)?;
    let directory = manifest
        .parent()
        .ok_or_else(|| rejected("Cargo manifest has no parent directory."))?;
    if let Some(path) = package.get("package").and_then(|p| p.get("workspace")) {
        return path
            .as_str()
            .map(|path| directory.join(path))
            .ok_or_else(|| rejected("Cargo package.workspace is not a path."));
    }
    if package.get("workspace").is_some() {
        return Ok(directory.to_path_buf());
    }
    for parent in directory.ancestors().skip(1) {
        let manifest = parent.join("Cargo.toml");
        if config_exists(&manifest)? && read_toml(&manifest, budget)?.get("workspace").is_some() {
            return Ok(parent.to_path_buf());
        }
    }
    Ok(directory.to_path_buf())
}

fn resolved(path: &Path) -> PathBuf {
    // Resolve the longest existing ancestor, including junctions, then append
    // absent children. Lexical normalization alone would miss a lane alias.
    if let Ok(path) = dunce::canonicalize(path) {
        return path;
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    for ancestor in result.ancestors() {
        if let Ok(base) = dunce::canonicalize(ancestor) {
            return base.join(result.strip_prefix(ancestor).unwrap_or(Path::new("")));
        }
    }
    result
}

fn managed_target(path: &Path) -> Result<Option<PathBuf>, String> {
    let path = resolved(path);
    for root in path.ancestors() {
        // The existing lane owner publishes this marker for default and custom
        // roots. Honor it anywhere, independent of checkout, crate or lane name.
        // Presence reserves the tree even if its contents are malformed; only
        // the owning runner may validate or repair its marker.
        if config_exists(&root.join(".codex-cargo-lanes-root"))? {
            return Ok(Some(root.to_path_buf()));
        }
        // Preserve the pre-marker core lane reservation in older checkouts.
        if root.join("codex-rs/.config/kd4-rust-tests.toml").is_file() {
            let lane = resolved(&root.join("codex-rs/target/lanes/core-tests"));
            let protected = if cfg!(windows) {
                Path::new(&path.to_string_lossy().to_ascii_lowercase())
                    .starts_with(lane.to_string_lossy().to_ascii_lowercase())
            } else {
                path.starts_with(&lane)
            };
            if protected {
                return Ok(Some(lane));
            }
        }
    }
    Ok(None)
}

fn raw_build(argv: &[String]) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    if !program_name(program).eq_ignore_ascii_case("cargo") {
        return false;
    }
    let mut tokens = args.iter().take_while(|arg| arg.as_str() != "--");
    let mut subcommand = None;
    while let Some(arg) = tokens.next() {
        if matches!(arg.as_str(), "--help" | "-h" | "--version" | "-V") {
            return false;
        }
        if matches!(
            arg.as_str(),
            "--config" | "--color" | "-C" | "-Z" | "--manifest-path" | "--target-dir"
        ) {
            tokens.next();
        } else if !arg.starts_with(['-', '+']) && subcommand.is_none() {
            subcommand = Some(arg.as_str());
        }
    }
    // nextest list compiles too; `cargo test --no-run/-- --list` also compiles.
    if subcommand == Some("nextest")
        && args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .any(|arg| arg == "--binaries-metadata" || arg.starts_with("--binaries-metadata="))
    {
        return false;
    }
    // Unknown aliases/plugins may build; do not infer read-only from their name.
    subcommand.is_some_and(|name| {
        !matches!(
            name,
            "help"
                | "version"
                | "metadata"
                | "tree"
                | "locate-project"
                | "pkgid"
                | "search"
                | "info"
                | "fetch"
                | "vendor"
                | "update"
                | "generate-lockfile"
                | "fmt"
        )
    })
}

fn cargo_subcommand(args: &[String]) -> Option<(usize, &str)> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            return None;
        }
        if matches!(
            arg.as_str(),
            "--config" | "--color" | "-C" | "-Z" | "--manifest-path" | "--target-dir"
        ) {
            index += 2;
        } else if arg.starts_with(['-', '+']) {
            index += 1;
        } else {
            return Some((index, arg));
        }
    }
    None
}

fn cargo_builtin(name: &str) -> bool {
    // Built-in command names cannot be redefined; shorthand aliases and
    // external commands (including fmt/nextest/clippy) can be overridden.
    matches!(
        name,
        "add"
            | "bench"
            | "build"
            | "check"
            | "clean"
            | "config"
            | "doc"
            | "fetch"
            | "fix"
            | "generate-lockfile"
            | "help"
            | "info"
            | "init"
            | "install"
            | "locate-project"
            | "login"
            | "logout"
            | "metadata"
            | "new"
            | "owner"
            | "package"
            | "pkgid"
            | "publish"
            | "read-manifest"
            | "remove"
            | "report"
            | "run"
            | "rustc"
            | "rustdoc"
            | "search"
            | "test"
            | "tree"
            | "uninstall"
            | "update"
            | "verify-project"
            | "version"
            | "yank"
    )
}

fn potential_build(argv: &[String]) -> bool {
    // Opaque shell commands cannot assume a plugin/help invocation is read-only
    // until configured aliases have been expanded in a known environment.
    raw_build(argv)
        || argv
            .first()
            .is_some_and(|program| program_name(program).eq_ignore_ascii_case("cargo"))
            && cargo_subcommand(&argv[1..]).is_some_and(|(_, name)| !cargo_builtin(name))
}

/// Programs that only report on existing processes or text. None of them can
/// start a build, so a Cargo name appearing in their filters or arguments is
/// data, not an invocation, and must not reach the build rejection.
fn read_only_inspection_program(program: &str) -> bool {
    matches!(
        program_name(program).to_ascii_lowercase().as_str(),
        "rg" | "rga"
            | "echo"
            | "write-output"
            | "get-content"
            | "select-string"
            | "get-process"
            | "get-ciminstance"
            | "get-wmiobject"
            | "tasklist"
            | "ps"
            | "pgrep"
            | "pidof"
    )
}

fn visibly_builds(script: &str) -> bool {
    // Statement/block separators are retained as words so a Cargo name can
    // be told apart from a Cargo invocation: only a name in command position
    // starts a build. Quoted newlines/braces remain data; preserve the existing
    // treatment of other operators, including command substitutions. Commas
    // and assignments separate words, not commands, so process lists such as
    // `Get-Process cargo,rustc` do not become invocations.
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for ch in script.chars() {
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            }
        }
        if matches!(ch, ';' | '|' | '&' | '(' | ')')
            || (quote.is_none() && matches!(ch, '{' | '}' | '\r' | '\n'))
        {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            words.push(ch.to_string());
        } else if ch.is_whitespace() || matches!(ch, '{' | '}' | ',' | '=' | '\'' | '"') {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words.iter().enumerate().any(|(index, word)| {
        if !program_name(word).eq_ignore_ascii_case("cargo") {
            return false;
        }
        // Command position: script start, or just after a separator or a
        // PowerShell/sh call operator.
        let in_command_position = match index.checked_sub(1).and_then(|prev| words.get(prev)) {
            None => true,
            Some(previous) => matches!(
                previous.as_str(),
                ";" | "|"
                    | "&"
                    | "("
                    | ")"
                    | "{"
                    | "}"
                    | "\r"
                    | "\n"
                    | "&&"
                    | "||"
                    | "."
                    | "exec"
                    | "command"
            ),
        };
        if !in_command_position {
            return false;
        }
        let rest = words[index..]
            .iter()
            .take_while(|word| {
                !matches!(
                    word.as_str(),
                    ";" | "|" | "&" | "(" | ")" | "{" | "}" | "\r" | "\n"
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        potential_build(&rest)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("codex-rs/.config")).unwrap();
        std::fs::write(root.path().join("codex-rs/.config/kd4-rust-tests.toml"), "").unwrap();
        root
    }

    #[test]
    fn ordinary_workspace_documents_and_managed_markers() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("independent-workspace");
        let member = workspace.join("member");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::create_dir_all(workspace.join(".cargo")).unwrap();
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\nmembers=['member']\n",
        )
        .unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname='member'\nversion='0.1.0'\n",
        )
        .unwrap();
        // Built component-wise like Cargo's own lookup, so its display matches the
        // native-separator path the rejection names.
        let config = workspace.join(".cargo").join("config.toml");
        std::fs::write(
            &config,
            "[env]\nRUST_TEST_THREADS={value='8',force=false}\n",
        )
        .unwrap();
        let env = HashMap::from([(
            "CARGO_HOME".into(),
            root.path().join("cargo-home").display().to_string(),
        )]);
        let command = ["cargo", "test", "--locked"].map(str::to_string);
        for cwd in [&workspace, &member] {
            assert!(check(&command, cwd, &env, 0).is_ok());
        }
        assert!(!workspace.join("target").exists());
        // A marker reserves an arbitrary workspace's default target tree,
        // including member invocations. Malformed contents remain reserved.
        std::fs::create_dir_all(workspace.join("target")).unwrap();
        std::fs::write(
            workspace.join("target/.codex-cargo-lanes-root"),
            "malformed",
        )
        .unwrap();
        let error = check(&command, &member, &env, 0).unwrap_err();
        assert!(error.contains("managed Cargo directory"));
        assert!(!workspace.join("target/debug").exists());
        for target in ["../target-sibling", "../target/../ordinary-output"] {
            let command = ["cargo", "build", "--target-dir", target].map(str::to_string);
            assert!(check(&command, &member, &env, 0).is_ok());
        }
        std::fs::write(&config, "[env\n").unwrap();
        let error = check(&command, &member, &env, 0).unwrap_err();
        assert!(error.contains(&config.display().to_string()));
        assert!(!error.contains("core-tests"));
        assert!(!error.contains("rust_test_runner.py"));
    }

    #[tokio::test]
    async fn runtime_admission_rejects_before_execution() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        let error = check_managed_cargo_lane(
            &["cargo".into(), "build".into()],
            &cwd,
            &HashMap::from([("CARGO_TARGET_DIR".into(), "target/lanes/core-tests".into())]),
        )
        .await
        .unwrap_err();
        assert!(error.contains("no build started"));
        assert!(error.contains("managed build/test entrypoint"));
        assert!(!cwd.join("target").exists());
    }

    #[test]
    fn effective_target_and_inspection() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        for target in [
            "target/lanes/core-tests".to_string(),
            "target/lanes/other/../core-tests".into(),
            cwd.join("target/lanes/core-tests").display().to_string(),
        ] {
            let env = HashMap::from([("CARGO_TARGET_DIR".into(), target.clone())]);
            for args in [
                vec!["cargo", "build"],
                vec!["cargo", "nextest", "list"],
                vec!["cargo", "test", "--no-run"],
            ] {
                let command = args.into_iter().map(str::to_string).collect::<Vec<_>>();
                assert!(
                    check(&command, &cwd, &env, 0)
                        .unwrap_err()
                        .contains("no build started")
                );
            }
            assert!(
                check(
                    &[
                        "cargo".into(),
                        "build".into(),
                        "--target-dir".into(),
                        target
                    ],
                    &cwd,
                    &HashMap::new(),
                    0
                )
                .is_err()
            );
            for args in [
                vec!["cargo", "metadata", "--no-deps"],
                vec!["cargo", "build", "--help"],
                vec!["cargo", "build", "--target-dir=target/unrelated"],
                vec![
                    "python",
                    "-I",
                    "-B",
                    "scripts/build/rust_test_runner.py",
                    "run-target",
                    "core_lib",
                ],
                vec!["just", "core-test-fast", "core_lib"],
            ] {
                assert!(
                    check(
                        &args.into_iter().map(str::to_string).collect::<Vec<_>>(),
                        &cwd,
                        &env,
                        0
                    )
                    .is_ok()
                );
            }
        }
    }

    #[test]
    fn plugins_are_admitted_by_directories_not_their_names() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        let env = HashMap::from([("CARGO_TARGET_DIR".into(), "target/lanes/core-tests".into())]);
        for plugin in ["llvm-cov", "custom-plugin"] {
            let command = ["cargo", plugin, "--target-dir", "target/unrelated"].map(str::to_string);
            assert!(check(&command, &cwd, &env, 0).is_ok());
            assert!(check(&command[..2], &cwd, &env, 0).is_err());
        }
    }

    #[test]
    fn aliases_cannot_hide_output_flags_or_override_builtins() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        std::fs::create_dir_all(cwd.join(".cargo")).unwrap();
        std::fs::write(cwd.join(".cargo/config.toml"),
            "[alias]\nprotected='build --target-dir target/lanes/core-tests'\nindirect='protected'\nunrelated=['build','--target-dir','target/unrelated']\nfmt='protected'\nloopback='loopback'\nbuild='metadata'\n").unwrap();
        for name in ["protected", "indirect", "fmt", "loopback"] {
            assert!(
                check(&["cargo".into(), name.into()], &cwd, &HashMap::new(), 0).is_err(),
                "{name}"
            );
        }
        assert!(
            check(
                &["cargo".into(), "unrelated".into()],
                &cwd,
                &HashMap::new(),
                0
            )
            .is_ok()
        );
        let env = HashMap::from([(
            "CARGO_ALIAS_UNRELATED".into(),
            "build --target-dir target/lanes/core-tests".into(),
        )]);
        assert!(check(&["cargo".into(), "unrelated".into()], &cwd, &env, 0).is_err());
        assert!(
            check(
                &[
                    "cargo".into(),
                    "build".into(),
                    "--target-dir=target/lanes/core-tests".into()
                ],
                &cwd,
                &HashMap::new(),
                0
            )
            .is_err()
        );
        // Cargo 1.98.1 expands the file alias despite this later CLI override.
        let command = [
            "cargo",
            "--config",
            "alias.protected='metadata'",
            "protected",
        ]
        .map(str::to_string);
        assert!(check(&command, &cwd, &HashMap::new(), 0).is_err());
    }

    #[test]
    fn included_directories_keep_their_origin_and_merge_order() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        let config = cwd.join(".cargo/config.toml");
        std::fs::create_dir_all(cwd.join(".cargo/nested")).unwrap();
        std::fs::write(
            cwd.join(".cargo/nested/leaf.toml"),
            "[build]\ntarget-dir='../target/lanes/core-tests'\n",
        )
        .unwrap();
        std::fs::write(
            &config,
            "include=[{path='absent.toml',optional=true},'nested/leaf.toml']\n",
        )
        .unwrap();
        let command = ["cargo".into(), "build".into()];
        // Cargo metadata independently places a nested include's directory
        // relative to .cargo, not relative to the outer config or current cwd.
        assert!(check(&command, &cwd, &HashMap::new(), 0).is_err());
        std::fs::write(
            cwd.join(".cargo/later.toml"),
            "[build]\ntarget-dir='target/unrelated'\n",
        )
        .unwrap();
        std::fs::write(&config, "include=['nested/leaf.toml','later.toml']\n").unwrap();
        assert!(check(&command, &cwd, &HashMap::new(), 0).is_ok());
        std::fs::write(
            &config,
            "include=['later.toml','nested/leaf.toml']\n[build]\ntarget-dir='target/unrelated'\n",
        )
        .unwrap();
        assert!(check(&command, &cwd, &HashMap::new(), 0).is_ok());
        std::fs::write(&config, "").unwrap();
        let command = [
            "cargo",
            "--config",
            "include=['.cargo/nested/leaf.toml']",
            "build",
        ]
        .map(str::to_string);
        assert!(check(&command, &cwd, &HashMap::new(), 0).is_err());
    }

    #[test]
    fn intermediate_directory_cannot_hide_behind_an_unrelated_target() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        let command =
            ["cargo", "custom-plugin", "--target-dir=target/unrelated"].map(str::to_string);
        let env = HashMap::from([(
            "CARGO_BUILD_BUILD_DIR".into(),
            "target/lanes/core-tests".into(),
        )]);
        assert!(
            check(&command, &cwd, &env, 0)
                .unwrap_err()
                .contains("Effective build directory")
        );
        let mut override_command = command.to_vec();
        override_command.extend([
            "--config".into(),
            "build.build-dir='target/other-intermediate'".into(),
        ]);
        assert!(check(&override_command, &cwd, &env, 0).is_ok());

        std::fs::create_dir_all(cwd.join(".cargo/nested")).unwrap();
        std::fs::write(
            cwd.join(".cargo/config.toml"),
            "include=['nested/build.toml']\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join(".cargo/nested/build.toml"),
            "[build]\nbuild-dir='../target/lanes/core-tests'\n",
        )
        .unwrap();
        assert!(check(&command, &cwd, &HashMap::new(), 0).is_err());
        assert!(check(&override_command, &cwd, &HashMap::new(), 0).is_ok());
    }

    #[test]
    fn target_environment_precedence_matches_cargo_metadata_probe() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        let command = [
            "cargo",
            "--config",
            "build.target-dir='target/unrelated'",
            "build",
        ]
        .map(str::to_string);
        for (key, allowed) in [
            ("CARGO_TARGET_DIR", false),
            ("CARGO_BUILD_TARGET_DIR", true),
        ] {
            let env = HashMap::from([(key.into(), "target/lanes/core-tests".into())]);
            assert_eq!(check(&command, &cwd, &env, 0).is_ok(), allowed, "{key}");
        }
    }

    #[test]
    fn build_templates_use_workspace_and_cargo_home_not_package_cwd() {
        let root = fixture();
        let workspace = root.path().join("codex-rs");
        let package = workspace.join("member");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\nmembers=['member']\n",
        )
        .unwrap();
        std::fs::write(
            package.join("Cargo.toml"),
            "[package]\nname='member'\nversion='0.0.0'\n",
        )
        .unwrap();
        let command = ["cargo", "build", "--target-dir=unrelated"].map(str::to_string);
        let mut env = HashMap::from([(
            "CARGO_BUILD_BUILD_DIR".into(),
            "{workspace-root}/target/lanes/core-tests".into(),
        )]);
        assert!(check(&command, &package, &env, 0).is_err());
        env.insert(
            "CARGO_BUILD_BUILD_DIR".into(),
            "{cargo-cache-home}/intermediate".into(),
        );
        env.insert(
            "CARGO_HOME".into(),
            workspace
                .join("target/lanes/core-tests")
                .display()
                .to_string(),
        );
        assert!(check(&command, &package, &env, 0).is_err());
        env.insert(
            "CARGO_BUILD_BUILD_DIR".into(),
            "{workspace-path-hash}".into(),
        );
        assert!(
            check(&command, &package, &env, 0)
                .unwrap_err()
                .contains("template")
        );
    }

    #[test]
    fn config_cycles_and_excessive_reads_fail_closed() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        std::fs::create_dir_all(cwd.join(".cargo")).unwrap();
        let config = cwd.join(".cargo/config.toml");
        let command = ["cargo", "build", "--target-dir=unrelated"].map(str::to_string);
        std::fs::write(&config, "include=['config.toml']\n").unwrap();
        assert!(
            check(&command, &cwd, &HashMap::new(), 0)
                .unwrap_err()
                .contains("cycle")
        );
        std::fs::write(&config, " ".repeat(1024 * 1024 + 1)).unwrap();
        assert!(
            check(&command, &cwd, &HashMap::new(), 0)
                .unwrap_err()
                .contains("bounded admission read")
        );
        std::fs::write(&config, "include=['missing.toml']\n").unwrap();
        assert!(check(&command, &cwd, &HashMap::new(), 0).is_err());
    }

    #[test]
    fn shell_and_config_targets_are_not_a_bypass() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        for script in [
            "cargo build --target-dir target/lanes/core-tests",
            "env CARGO_TARGET_DIR=target/lanes/core-tests cargo build",
            "export CARGO_TARGET_DIR=target/lanes/core-tests; cargo build",
            "sh -c 'cargo build --target-dir target/lanes/core-tests'",
        ] {
            assert!(
                check(
                    &["sh".into(), "-c".into(), script.into()],
                    &cwd,
                    &HashMap::new(),
                    0
                )
                .is_err(),
                "{script}"
            );
        }
        assert!(
            check(
                &[
                    "sh".into(),
                    "-c".into(),
                    "env CARGO_TARGET_DIR=target/other cargo build".into()
                ],
                &cwd,
                &HashMap::new(),
                0
            )
            .is_ok()
        );
        std::fs::create_dir_all(cwd.join(".cargo")).unwrap();
        std::fs::write(
            cwd.join(".cargo/config.toml"),
            "[build]\ntarget-dir='target/lanes/core-tests'\n",
        )
        .unwrap();
        assert!(check(&["cargo".into(), "build".into()], &cwd, &HashMap::new(), 0).is_err());
        assert!(
            check(
                &[
                    "cargo".into(),
                    "build".into(),
                    "--target-dir=target/other".into()
                ],
                &cwd,
                &HashMap::new(),
                0
            )
            .is_ok()
        );
    }

    #[test]
    fn windows_shell_indirection_is_rejected_before_launch() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        for (shell, switch, script) in [
            (
                "cmd",
                "/c",
                "set CARGO_TARGET_DIR=target/lanes/core-tests&& cargo build",
            ),
            (
                "pwsh",
                "-Command",
                "$env:CARGO_TARGET_DIR='target/lanes/core-tests'; cargo build",
            ),
            (
                "pwsh",
                "-Command",
                "& 'cargo' build --target-dir target/lanes/core-tests",
            ),
            (
                "pwsh",
                "-Command",
                "Start-Process cargo -ArgumentList 'build','--target-dir','target/lanes/core-tests'",
            ),
        ] {
            assert!(
                check(
                    &[shell.into(), switch.into(), script.into()],
                    &cwd,
                    &HashMap::new(),
                    0
                )
                .is_err(),
                "{script}"
            );
        }
    }

    #[test]
    fn opaque_powershell_statement_boundaries_cannot_bypass_admission() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        for script in [
            "$env:CARGO_TARGET_DIR='target/lanes/core-tests'\ncargo build",
            "$env:CARGO_TARGET_DIR='target/lanes/core-tests'\r\ncargo build",
            "$env:CARGO_TARGET_DIR='target/lanes/core-tests'; try { cargo build } finally {}",
            "$env:CARGO_TARGET_DIR='target/lanes/core-tests'\ncargo build\nWrite-Output '--help'",
            "$env:CARGO_TARGET_DIR='target/lanes/core-tests'; Write-Output \"$(cargo build)\"",
        ] {
            let command = ["pwsh".into(), "-Command".into(), script.into()];
            assert!(
                argv_commands(&command, Some(ShellType::PowerShell)).is_none(),
                "regression must exercise the opaque-script fallback: {script}"
            );
            let error = check(&command, &cwd, &HashMap::new(), 0)
                .expect_err("shell-local targets must not bypass managed-directory admission");
            assert!(
                error.contains("Raw Cargo launch rejected; no build started"),
                "{script}: {error}"
            );
        }
    }

    #[test]
    fn opaque_powershell_quoted_examples_are_not_builds() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        for script in [
            "$env:EXAMPLE='example\ncargo build'; Write-Output $env:EXAMPLE",
            "$env:EXAMPLE=\"example\r\ncargo build\"; Write-Output $env:EXAMPLE",
            "$env:EXAMPLE='try { cargo build }'; Write-Output $env:EXAMPLE",
            "$env:EXAMPLE='processes'; Get-Process cargo,rustc -ErrorAction SilentlyContinue",
        ] {
            let command = ["pwsh".into(), "-Command".into(), script.into()];
            assert!(
                argv_commands(&command, Some(ShellType::PowerShell)).is_none(),
                "positive control must exercise the opaque-script fallback: {script}"
            );
            assert!(
                check(&command, &cwd, &HashMap::new(), 0).is_ok(),
                "{script}"
            );
        }
    }

    #[test]
    fn read_only_process_inspection_naming_cargo_is_admitted() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        for script in [
            // A Cargo name inside a regex is data, not an invocation.
            "Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'cargo|rustc' }",
            // A comma-joined process-name list must not read as `cargo rustc`.
            "Get-Process cargo,rustc -ErrorAction SilentlyContinue | Select-Object Id,ProcessName",
            "Get-Process -Name cargo,rustc,rustdoc,cargo-nextest | ConvertTo-Json -Compress",
            "tasklist | Select-String cargo",
            "Get-Process | Where-Object { $_.Name -eq 'cargo' }",
        ] {
            assert!(
                check(
                    &["pwsh".into(), "-Command".into(), script.into()],
                    &cwd,
                    &HashMap::new(),
                    0
                )
                .is_ok(),
                "read-only process inspection must not be rejected: {script}"
            );
        }
    }

    #[test]
    fn inspection_clause_does_not_block_its_unrelated_siblings() {
        let root = fixture();
        let cwd = root.path().join("codex-rs");
        // The process query previously rejected the whole command, taking an
        // unrelated instruction/source search down with it.
        let script = "Get-Process cargo,rustc -ErrorAction SilentlyContinue; rg -n 'core-test-gates' AGENTS.MD";
        assert!(
            check(
                &["pwsh".into(), "-Command".into(), script.into()],
                &cwd,
                &HashMap::new(),
                0
            )
            .is_ok(),
            "{script}"
        );
        // A genuine build clause is still rejected, and names itself.
        let building = "Get-Process cargo; cargo build";
        let error = check(
            &["pwsh".into(), "-Command".into(), building.into()],
            &cwd,
            &HashMap::new(),
            0,
        )
        .expect_err("a real build clause must still be rejected");
        assert!(
            error.contains("cargo build"),
            "rejection should name the offending clause: {error}"
        );
    }

    #[test]
    fn build_rejection_remediation_follows_the_lane_marker() {
        // A marked tree must not be told that no repository-specific runner is
        // needed; an unmarked workspace must keep the neutral advice.
        let marked = tempfile::tempdir().unwrap();
        let workspace = marked.path().join("reserved");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join(".codex-cargo-lanes-root"), "").unwrap();
        let reserved = rejected_build("Detail.", &workspace);
        assert!(
            reserved.contains("managed test/build entrypoint"),
            "marked tree should point at its managed entrypoint: {reserved}"
        );
        assert!(
            !reserved.contains("ordinary Rust workspaces do not require"),
            "marked tree must not claim no runner is needed: {reserved}"
        );

        let plain = tempfile::tempdir().unwrap();
        let unmarked = plain.path().join("independent");
        std::fs::create_dir_all(&unmarked).unwrap();
        std::fs::write(
            unmarked.join("Cargo.toml"),
            "[package]\nname='independent'\nversion='0.1.0'\n",
        )
        .unwrap();
        let neutral = rejected_build("Detail.", &unmarked);
        assert!(
            neutral
                .contains("ordinary Rust workspaces do not require a repository-specific runner"),
            "unmarked workspace keeps neutral advice: {neutral}"
        );
    }
}
