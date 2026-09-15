use super::provenance::FileIdentity;
use super::provenance::command_output;
use super::provenance::hash_bytes;
use crate::schedule::Variant;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

pub const BASE_CONFIG: &str = r#"approval_policy = "never"
sandbox_mode = "danger-full-access"
personality = "pragmatic"
model = "gpt-6-astra"
model_reasoning_effort = "high"
plan_mode_reasoning_effort = "ultra"
model_verbosity = "low"
model_reasoning_summary = "concise"
model_auto_compact_token_limit = 129000
model_auto_compact_token_limit_scope = "total"
"#;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDifferences {
    pub project_only: Vec<String>,
    pub benchmark_only: Vec<String>,
    pub changed: Vec<String>,
    pub matching_keys: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectConfigComparison {
    pub path: PathBuf,
    pub sha256: String,
    pub by_variant: BTreeMap<Variant, ConfigDifferences>,
}

pub(super) fn configured_value(base: &str, overrides: &[String]) -> Result<serde_json::Value> {
    let mut config: toml::Value = toml::from_str(base)?;
    for setting in overrides {
        let parsed: toml::Value = toml::from_str(setting)?;
        if let Some(features) = parsed.get("features").and_then(toml::Value::as_table) {
            let table = config
                .as_table_mut()
                .context("config table")?
                .entry("features")
                .or_insert_with(|| toml::Value::Table(Default::default()))
                .as_table_mut()
                .context("features table")?;
            table.extend(features.clone());
        }
    }
    Ok(serde_json::to_value(config)?)
}

impl ProjectConfigComparison {
    /// Freeze explicit project settings, not the layered daily effective config.
    /// Persist only key names and a content hash; values can contain credentials.
    pub fn capture(
        repo: &Path,
        base: &str,
        overrides: &BTreeMap<Variant, Vec<String>>,
    ) -> Result<Option<Self>> {
        let path = repo.join(".codex/config.toml");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("read project config comparison"),
        };
        let project: toml::Value = toml::from_str(std::str::from_utf8(&bytes)?)
            .context("parse project config comparison")?;
        fn leaves(
            value: &serde_json::Value,
            prefix: &str,
            result: &mut BTreeMap<String, serde_json::Value>,
        ) {
            if let Some(table) = value.as_object() {
                for (key, value) in table {
                    let key = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    leaves(value, &key, result);
                }
            } else {
                result.insert(prefix.into(), value.clone());
            }
        }
        let mut project_keys = BTreeMap::new();
        leaves(&serde_json::to_value(project)?, "", &mut project_keys);
        let mut by_variant = BTreeMap::new();
        for variant in Variant::ALL {
            let mut benchmark_keys = BTreeMap::new();
            leaves(
                &configured_value(base, &overrides[&variant])?,
                "",
                &mut benchmark_keys,
            );
            by_variant.insert(
                variant,
                ConfigDifferences {
                    project_only: project_keys
                        .keys()
                        .filter(|key| !benchmark_keys.contains_key(*key))
                        .cloned()
                        .collect(),
                    benchmark_only: benchmark_keys
                        .keys()
                        .filter(|key| !project_keys.contains_key(*key))
                        .cloned()
                        .collect(),
                    changed: project_keys
                        .iter()
                        .filter(|(key, value)| {
                            benchmark_keys
                                .get(*key)
                                .is_some_and(|other| other != *value)
                        })
                        .map(|(key, _)| key.clone())
                        .collect(),
                    matching_keys: project_keys
                        .iter()
                        .filter(|(key, value)| benchmark_keys.get(*key) == Some(*value))
                        .count(),
                },
            );
        }
        Ok(Some(Self {
            path,
            sha256: hash_bytes(&bytes),
            by_variant,
        }))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolIdentity {
    pub executable: FileIdentity,
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub variables: BTreeMap<String, String>,
    pub tools: BTreeMap<String, ToolIdentity>,
    pub rust_toolchain: String,
}

// Match the native Windows shell discovery order, including installations that
// are absent from PATH. A newly installed preferred shell invalidates preparation.
pub(super) fn select_windows_shell(
    mut find_binary: impl FnMut(&str) -> Option<PathBuf>,
    mut is_file: impl FnMut(&Path) -> bool,
) -> Result<PathBuf> {
    for (name, fallback) in [
        ("pwsh", r"C:\Program Files\PowerShell\7\pwsh.exe"),
        (
            "powershell",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        ),
    ] {
        if let Some(path) = find_binary(name) {
            return Ok(path);
        }
        let path = Path::new(fallback);
        if is_file(path) {
            return Ok(path.to_path_buf());
        }
    }
    anyhow::bail!("no supported native PowerShell host is available")
}

fn native_shell() -> Result<PathBuf> {
    if cfg!(windows) {
        select_windows_shell(|name| which::which(name).ok(), Path::is_file)
    } else {
        which::which("sh").context("missing required shell")
    }
}

impl Environment {
    pub fn capture(repo: &Path) -> Result<Self> {
        // Explicit allowlist: authentication and unrelated runtime overrides are never persisted.
        let names = [
            "PATH",
            "PATHEXT",
            "SystemRoot",
            "WINDIR",
            "COMSPEC",
            "TEMP",
            "TMP",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "LOCALAPPDATA",
            "APPDATA",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "SYSTEMDRIVE",
            "OS",
            "PROCESSOR_ARCHITECTURE",
            "NUMBER_OF_PROCESSORS",
            "RUSTUP_HOME",
            "CARGO_HOME",
        ];
        let variables = names
            .into_iter()
            .filter_map(|key| std::env::var(key).ok().map(|v| (key.to_owned(), v)))
            .collect();
        let toolchain: toml::Value = toml::from_str(&std::fs::read_to_string(
            repo.join("codex-rs/rust-toolchain.toml"),
        )?)?;
        let rust_toolchain = toolchain["toolchain"]["channel"]
            .as_str()
            .context("pinned Rust toolchain")?
            .to_owned();
        let mut tools = BTreeMap::new();
        for name in ["python", "node", "rustc", "cargo", "git"] {
            let executable = FileIdentity::record(
                &which::which(name)
                    .with_context(|| format!("missing required executable {name}"))?,
            )?;
            let mut command = Command::new(&executable.path);
            command.arg("--version");
            let version = String::from_utf8(command_output(&mut command)?)?
                .trim()
                .to_owned();
            tools.insert(
                name.into(),
                ToolIdentity {
                    executable,
                    version,
                },
            );
        }
        let executable = FileIdentity::record(&native_shell()?)?;
        let mut command = Command::new(&executable.path);
        command.env_clear().envs(&variables);
        if cfg!(windows) {
            command.args([
                "-NoProfile",
                "-Command",
                "$PSVersionTable.PSVersion.ToString()",
            ]);
        } else {
            command.args(["-c", "echo $0"]);
        }
        let version = String::from_utf8(command_output(&mut command)?)?
            .trim()
            .to_owned();
        tools.insert(
            "shell".into(),
            ToolIdentity {
                executable,
                version,
            },
        );
        Ok(Self {
            variables,
            tools,
            rust_toolchain,
        })
    }

    pub fn verify(&self) -> Result<()> {
        for tool in self.tools.values() {
            tool.executable.verify()?;
        }
        for key in ["PATH", "PATHEXT"] {
            ensure!(
                std::env::var(key).ok().as_ref() == self.variables.get(key),
                "execution environment changed: {key}; prepare again"
            );
        }
        let shell = self
            .tools
            .get("shell")
            .context("prepared environment lacks native shell identity; prepare again")?;
        ensure!(
            std::fs::canonicalize(native_shell()?)? == shell.executable.path,
            "native shell selection changed; prepare again"
        );
        Ok(())
    }
}
