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
    // Decode identity before taking the non-Cargo path: shell quoting and
    // EncodedCommand can hide even a literal Cargo program from a raw scan.
    if infer_direct_shell_type(command).is_none()
        && command.first().is_none_or(|program| {
            !matches!(
                program_name(program).to_ascii_lowercase().as_str(),
                "cargo" | "env" | "command" | "time" | "exec" | "call" | "rustup"
                    | "start-process" | "invoke-expression" | "iex" | "start"
            )
        })
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
        if shell == ShellType::PowerShell
            && let Some(index) = args
                .iter()
                .take_while(|arg| {
                    !matches!(arg.to_ascii_lowercase().as_str(), "-command" | "-c" | "-file" | "-f")
                })
                .position(|arg| {
                    matches!(arg.to_ascii_lowercase().as_str(), "-encodedcommand" | "-enc" | "-ec" | "-e")
                })
        {
            use base64::Engine as _;
            let [encoded] = &args[index + 1..] else {
                return Err(rejected("Cannot resolve PowerShell EncodedCommand arguments."));
            };
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| rejected("Cannot decode PowerShell EncodedCommand."))?;
            if bytes.len() % 2 != 0 {
                return Err(rejected("PowerShell EncodedCommand is not UTF-16LE."));
            }
            let units = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>();
            let script = String::from_utf16(&units)
                .map_err(|_| rejected("PowerShell EncodedCommand is not UTF-16LE."))?;
            let mut decoded = command[..index + 1].to_vec();
            decoded.extend(["-Command".into(), script]);
            return check(&decoded, cwd, env, depth + 1);
        }
        let parsed = std::cell::OnceCell::new();
        let parsed = || parsed.get_or_init(|| argv_commands(command, Some(shell)));
        // A flattened command list cannot model assignments, cd, functions or
        // expansion. Only a standalone literal command proves its environment.
        let standalone = match shell {
            ShellType::PowerShell => codex_shell_command::powershell::extract_powershell_command(
                command,
            )
            .and_then(|(_, script)| {
                codex_shell_command::powershell::standalone_powershell_inspection_argv(script)
            }),
            ShellType::Bash | ShellType::Sh | ShellType::Zsh => command
                .last()
                .and_then(|script| codex_shell_command::validation::standalone_argv(script)),
            ShellType::Cmd => parsed()
                .as_ref()
                .filter(|stages| stages.len() == 1)
                .and_then(|stages| stages.first().cloned())
                .filter(|stage| !stage.iter().any(|arg| arg.contains(['%', '!']))),
        };
        if let Some(argv) = standalone {
            return check(&argv, cwd, env, depth + 1);
        }
        // Flattening cannot prove the effective cwd/environment of a build.
        // A failed precise lowering must not turn a visible build into admission.
        // CMD owns the complete tail after /c or /k, not each launch argument
        // separately. Cargo's program and subcommand can span those arguments.
        let possible_build = if shell == ShellType::Cmd {
            super::extract_cmd_command(command).map_or_else(
                || args.iter().any(|script| visibly_builds_cmd(script)),
                |script| visibly_builds_cmd(script.as_ref()),
            )
        } else {
            args.iter().any(|script| visibly_builds(script))
        };
        if possible_build
            || parsed().as_ref().is_some_and(|stages| {
                stages.iter().any(|stage| visibly_builds_argv(stage, 0))
            })
        {
            return Err(rejected_build(
                "Cannot resolve Cargo's effective shell state; use standalone literal argv.",
                cwd,
            ));
        }
        // Read-only commands remain admissible.
        if let Some(stages) = parsed() {
            for stage in stages {
                check(stage, cwd, env, depth + 1)?;
            }
        }
        return Ok(());
    }
    let binary = program_name(program).to_ascii_lowercase();
    if matches!(binary.as_str(), "env" | "command" | "time" | "exec" | "rustup") {
        use codex_shell_command::validation::ExecutionWrapper;
        let start = match codex_shell_command::validation::decode_execution_wrapper(&binary, args) {
            ExecutionWrapper::Child(index) => index,
            ExecutionWrapper::NonExecution => return Ok(()),
            ExecutionWrapper::Opaque => {
                if args
                    .iter()
                    .any(|arg| arg.to_ascii_lowercase().contains("cargo"))
                {
                    return Err(rejected("Unsupported execution wrapper; use literal Cargo argv."));
                }
                return Ok(());
            }
        };
        let mut child = env.clone();
        let mut child_cwd = cwd.to_path_buf();
        let mut index = 0;
        if binary == "exec" && args[..start].iter().any(|arg| arg == "-c") {
            child.clear();
        }
        let mut options = true;
        while binary == "env" && index < start {
            let arg = &args[index];
            if options && matches!(arg.as_str(), "-i" | "--ignore-environment" | "-") {
                child.clear();
                index += 1;
                continue;
            }
            if options
                && (matches!(arg.as_str(), "-u" | "--unset")
                    || arg.starts_with("--unset=")
                    || (arg.starts_with("-u") && arg.len() > 2))
            {
                let key = if let Some(key) = arg
                    .strip_prefix("--unset=")
                    .or_else(|| arg.strip_prefix("-u").filter(|key| !key.is_empty()))
                {
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
            if options
                && (matches!(arg.as_str(), "-C" | "--chdir")
                    || arg.starts_with("--chdir=")
                    || (arg.starts_with("-C") && arg.len() > 2))
            {
                let path = if let Some(path) = arg
                    .strip_prefix("--chdir=")
                    .or_else(|| arg.strip_prefix("-C").filter(|path| !path.is_empty()))
                {
                    path
                } else {
                    index += 1;
                    &args[index]
                };
                child_cwd = child_cwd.join(path);
                index += 1;
                continue;
            }
            if options && arg == "--" {
                options = false;
                index += 1;
                continue;
            }
            if let Some((key, value)) = arg.split_once('=') {
                options = false;
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
        return check(&args[start..], &child_cwd, &child, depth + 1);
    }
    if program_name(program).eq_ignore_ascii_case("call") {
        return check(
            args.strip_prefix(&["--".to_string()]).unwrap_or(args),
            cwd,
            env,
            depth + 1,
        );
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
    for (kind, path) in std::iter::once(("target", &target))
        .chain((build != target).then_some(("build", &build)))
    {
        if let Some(managed) = managed_target(path)? {
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
        if config_exists(&manifest)?
            && let Some(workspace) = read_toml(&manifest, budget)?.get("workspace")
        {
            let parent = resolved(parent)?;
            let directory = resolved(directory)?;
            let relative = directory
                .strip_prefix(&parent)
                .map_err(|_| rejected("Cannot prove Cargo workspace membership."))?;
            let matches = |key| -> Result<bool, String> {
                let Some(patterns) = workspace.get(key) else {
                    return Ok(false);
                };
                let patterns = patterns
                    .as_array()
                    .ok_or_else(|| rejected("Cargo workspace membership is not an array."))?;
                for pattern in patterns {
                    let pattern = pattern
                        .as_str()
                        .ok_or_else(|| rejected("Cargo workspace member is not a path."))?;
                    let pattern = glob::Pattern::new(pattern)
                        .map_err(|_| rejected("Cannot parse Cargo workspace member pattern."))?;
                    if pattern.matches_path_with(
                        relative,
                        glob::MatchOptions {
                            require_literal_separator: true,
                            ..glob::MatchOptions::new()
                        },
                    ) {
                        return Ok(true);
                    }
                }
                Ok(false)
            };
            if matches("exclude")? {
                return Ok(directory);
            }
            if matches("members")? {
                return Ok(parent);
            }
            // Cargo can infer members through path dependencies. Without the
            // dependency graph, an unmatched nested package is not proof that
            // the ancestor owns its default target directory.
            return Err(rejected("Cannot prove this package's Cargo workspace membership; use an explicit target directory."));
        }
    }
    Ok(directory.to_path_buf())
}

fn resolved(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| rejected("Cannot resolve the current directory."))?
            .join(path)
    };
    // Canonicalize the ORIGINAL existing prefix, before removing any '..'.
    // alias/../absent follows alias first, just as the filesystem does.
    for ancestor in absolute.ancestors() {
        if let Ok(mut base) = dunce::canonicalize(ancestor) {
            let suffix = absolute
                .strip_prefix(ancestor)
                .map_err(|_| rejected("Cannot resolve the Cargo directory suffix."))?;
            for component in suffix.components() {
                match component {
                    // Resolving through an absent component would require
                    // creating it first. Do not guess away a later symlink.
                    Component::ParentDir => {
                        return Err(rejected(
                            "Cannot prove ownership through an absent parent traversal.",
                        ));
                    }
                    Component::CurDir => {}
                    other => base.push(other.as_os_str()),
                }
            }
            return Ok(base);
        }
        // Permission failures/dangling links are not evidence of absence.
        if let Err(error) = std::fs::symlink_metadata(ancestor)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(rejected("Cannot establish Cargo directory ownership."));
        }
        if std::fs::symlink_metadata(ancestor).is_ok() {
            return Err(rejected("Cannot canonicalize an existing Cargo directory."));
        }
    }
    Err(rejected("Cannot establish Cargo directory ownership."))
}

fn managed_target(path: &Path) -> Result<Option<PathBuf>, String> {
    let path = resolved(path)?;
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
            let lane = resolved(&root.join("codex-rs/target/lanes/core-tests"))?;
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

fn visibly_builds_cmd(script: &str) -> bool {
    super::possible_cmd_commands(script).is_none_or(|commands| {
        commands.iter().any(|argv| visibly_builds_argv(argv, 0))
    })
}

fn visibly_builds(script: &str) -> bool {
    visibly_builds_at_depth(script, 0)
}

fn visibly_builds_at_depth(script: &str, depth: usize) -> bool {
    if depth > 4 {
        return true;
    }
    super::possible_powershell_commands(script)
        .iter()
        .any(|argv| visibly_builds_argv(argv, depth))
}

fn visibly_builds_argv(argv: &[String], depth: usize) -> bool {
    if depth > 4 {
        return true;
    }
    if potential_build(argv) {
        return true;
    }
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    let binary = program_name(program).to_ascii_lowercase();
    if matches!(binary.as_str(), "env" | "command" | "time" | "exec" | "rustup") {
        use codex_shell_command::validation::ExecutionWrapper;
        use codex_shell_command::validation::decode_execution_wrapper;
        return match decode_execution_wrapper(&binary, args) {
            ExecutionWrapper::Child(index) => visibly_builds_argv(&args[index..], depth + 1),
            ExecutionWrapper::NonExecution | ExecutionWrapper::Opaque => false,
        };
    }
    if binary == "call" {
        return visibly_builds_argv(
            args.strip_prefix(&["--".to_string()]).unwrap_or(args),
            depth + 1,
        );
    }
    matches!(binary.as_str(), "iex" | "invoke-expression")
        && args
            .iter()
            .any(|source| visibly_builds_at_depth(source, depth + 1))
}



