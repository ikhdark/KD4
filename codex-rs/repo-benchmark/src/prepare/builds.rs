use super::environment::Environment;
use super::provenance::{FileIdentity, git_path, hash_bytes, read_json, write_json};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildIdentity {
    pub revision: String,
    pub source: PathBuf,
    pub target_directory: PathBuf,
    pub settings: BTreeMap<String, String>,
    pub lockfile: FileIdentity,
    pub cargo_config: Option<FileIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v8_artifacts: Option<super::v8::V8Artifacts>,
    pub executables: BTreeMap<String, FileIdentity>,
    pub log: PathBuf,
    pub cache_key: String,
    /// Original build, including input hashing and Cargo artifact capture.
    pub build_elapsed_ms: u64,
    /// This preparation's cache lookup and verification; absent on a fresh build.
    pub cache_reuse_elapsed_ms: Option<u64>,
}

pub fn settings(environment: &Environment) -> BTreeMap<String, String> {
    let mut settings = BTreeMap::from([
        ("jobs".into(), "6".into()),
        ("profile".into(), "release".into()),
        ("CARGO_PROFILE_RELEASE_OPT_LEVEL".into(), "3".into()),
        ("CARGO_PROFILE_RELEASE_LTO".into(), "thin".into()),
        ("CARGO_PROFILE_RELEASE_CODEGEN_UNITS".into(), "4".into()),
        ("CARGO_PROFILE_RELEASE_INCREMENTAL".into(), "false".into()),
        ("CARGO_INCREMENTAL".into(), "0".into()),
        (
            "RUSTUP_TOOLCHAIN".into(),
            environment.rust_toolchain.clone(),
        ),
    ]);
    if let Ok(path) = which::which("sccache") {
        settings.insert("RUSTC_WRAPPER".into(), path.to_string_lossy().into());
    }
    // Preserve required local SDK/linker selection and make it part of cache identity.
    for (key, value) in std::env::vars() {
        if key == "RUSTFLAGS"
            || key == "CARGO_ENCODED_RUSTFLAGS"
            || key.starts_with("CARGO_TARGET_") && key.ends_with("_LINKER")
            || [
                "INCLUDE",
                "LIB",
                "LIBPATH",
                "VCToolsInstallDir",
                "WindowsSdkDir",
                "WindowsSDKVersion",
            ]
            .contains(&key.as_str())
        {
            settings.insert(key, value);
        }
    }
    settings
}

pub(super) fn validate_toolchain(declaration: &str, revision: &str, expected: &str) -> Result<()> {
    let declaration: toml::Value = toml::from_str(declaration)?;
    let channel = declaration
        .get("toolchain")
        .and_then(|value| value.get("channel"))
        .and_then(toml::Value::as_str)
        .context("native source lacks a Rust toolchain channel")?;
    ensure!(
        channel == expected,
        "native revision {revision} declares Rust {channel}, but the shared benchmark toolchain is {expected}; incompatible toolchain pins"
    );
    Ok(())
}

pub fn build(
    source: &Path,
    revision: &str,
    target_root: &Path,
    env: &Environment,
    requested: &[(&str, &str)],
) -> Result<BuildIdentity> {
    let started = Instant::now();
    let workspace = source.join("codex-rs");
    validate_toolchain(
        &fs::read_to_string(workspace.join("rust-toolchain.toml"))
            .with_context(|| format!("read native toolchain for revision {revision}"))?,
        revision,
        &env.rust_toolchain,
    )?;
    let lockfile = FileIdentity::record(&workspace.join("Cargo.lock"))?;
    let config_path = workspace.join(".cargo/config.toml");
    let cargo_config = config_path
        .is_file()
        .then(|| FileIdentity::record(&config_path))
        .transpose()?;
    let v8_artifacts = super::v8::prepare(source, revision, target_root, env, requested)
        .with_context(|| format!("prepare V8 dependency for revision {revision}"))?;
    let mut settings = settings(env);
    if let Some(artifacts) = &v8_artifacts {
        settings.extend(artifacts.settings());
    }
    let original_key = hash_bytes(&serde_json::to_vec(&(
        revision,
        &lockfile.sha256,
        cargo_config.as_ref().map(|file| &file.sha256),
        &settings,
        requested,
        &env.tools,
    ))?);
    // Preserve existing successful fork caches when no source-pinned V8 setup applies.
    let key = if let Some(artifacts) = &v8_artifacts {
        hash_bytes(&serde_json::to_vec(&(
            original_key,
            artifacts.fingerprint()?,
        ))?)
    } else {
        original_key
    };
    let target_directory = target_root.join(&key);
    fs::create_dir_all(&target_directory)?;
    let record = target_directory.join("repo-benchmark-build.json");
    if record.exists() {
        let mut previous: BuildIdentity = read_json(&record)?;
        ensure!(previous.cache_key == key, "build cache provenance mismatch");
        previous.verify()?;
        previous.cache_reuse_elapsed_ms = Some(started.elapsed().as_millis() as u64);
        eprintln!("Reusing verified native build at {revision}; Cargo build skipped");
        return Ok(previous);
    }
    let log = target_directory.join("build.log");
    let mut log_file = fs::File::create(&log)?;
    let mut command = cargo_build_command(
        &workspace,
        &target_directory,
        env,
        requested,
        &settings,
        v8_artifacts
            .as_ref()
            .map(|artifacts| artifacts.target.as_str()),
    );
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log_file.try_clone()?));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    eprintln!(
        "Building {} at {} with six Cargo jobs",
        requested
            .iter()
            .map(|(_, b)| *b)
            .collect::<Vec<_>>()
            .join(", "),
        revision
    );
    let mut child = command.spawn().context("start native Cargo build")?;
    let mut executables = BTreeMap::new();
    for line in BufReader::new(child.stdout.take().context("Cargo artifact stream")?).lines() {
        let line = line?;
        writeln!(log_file, "{line}")?;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if value["reason"] == "compiler-artifact" {
                if let (Some(name), Some(path)) = (
                    value.pointer("/target/name").and_then(|v| v.as_str()),
                    value["executable"].as_str(),
                ) {
                    if requested.iter().any(|(_, binary)| *binary == name) {
                        executables.insert(name.to_owned(), FileIdentity::record(Path::new(path))?);
                    }
                }
            }
        }
    }
    let status = child.wait()?;
    ensure!(
        status.success(),
        "native build failed for revision {revision}; complete Cargo output: {}",
        log.display()
    );
    for (_, binary) in requested {
        ensure!(
            executables.contains_key(*binary),
            "Cargo produced no {binary} executable for revision {revision}"
        );
    }
    let result = BuildIdentity {
        revision: revision.into(),
        source: source.into(),
        target_directory,
        settings,
        lockfile,
        cargo_config,
        v8_artifacts,
        executables,
        log,
        cache_key: key,
        build_elapsed_ms: started.elapsed().as_millis() as u64,
        cache_reuse_elapsed_ms: None,
    };
    write_json(&record, &result)?;
    Ok(result)
}

fn cargo_build_command(
    workspace: &Path,
    target_directory: &Path,
    env: &Environment,
    requested: &[(&str, &str)],
    settings: &BTreeMap<String, String>,
    v8_target: Option<&str>,
) -> Command {
    let mut command = Command::new(&env.tools["cargo"].executable.path);
    command
        .current_dir(&workspace)
        .args([
            "build",
            "--release",
            "--locked",
            "--jobs",
            "6",
            "--message-format=json-render-diagnostics",
            "--target-dir",
        ])
        // MSVC's linker also interprets a verbatim prefix as wildcard syntax.
        // Pass Cargo the ordinary spelling of the same prepared directory.
        .arg(git_path(&target_directory));
    for (package, binary) in requested {
        command.args(["-p", package, "--bin", binary]);
    }
    command.env_remove("CARGO_TARGET_DIR");
    super::v8::clear_inherited_overrides(&mut command);
    if let Some(target) = v8_target {
        command.env_remove("CARGO_BUILD_TARGET");
        command.args(["--target", target]);
    }
    for (key, value) in settings {
        if key != "jobs" && key != "profile" {
            command.env(key, value);
        }
    }
    command
}

impl BuildIdentity {
    pub fn verify(&self) -> Result<()> {
        self.lockfile.verify()?;
        if let Some(config) = &self.cargo_config {
            config.verify()?;
        }
        if let Some(artifacts) = &self.v8_artifacts {
            artifacts.verify()?;
        }
        for executable in self.executables.values() {
            executable.verify()?;
        }
        ensure!(
            !self.executables.is_empty(),
            "build has no executable artifacts"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prepare::environment::ToolIdentity;
    use std::ffi::OsStr;

    #[test]
    fn actual_cargo_command_clears_inherited_v8_inputs_before_verified_overrides() -> Result<()> {
        const CHILD: &str = "REPO_BENCHMARK_V8_COMMAND_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Isolate parent-environment injection from other parallel tests.
            let output = Command::new(std::env::current_exe()?)
                .args(["--exact", "prepare::builds::tests::actual_cargo_command_clears_inherited_v8_inputs_before_verified_overrides", "--nocapture"])
                .env(CHILD, "1")
                .env("RUSTY_V8_ARCHIVE", "untrusted-archive")
                .env("RUSTY_V8_SRC_BINDING_PATH", "untrusted-binding")
                .env("RUSTY_V8_MIRROR", "untrusted-mirror")
                .env("V8_FROM_SOURCE", "1")
                .env("GN_ARGS", "unrecorded-native-settings")
                .env("CARGO_BUILD_TARGET", "wrong-target")
                .output()?;
            assert!(
                output.status.success(),
                "inherited-environment child failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return Ok(());
        }
        assert_eq!(std::env::var("RUSTY_V8_ARCHIVE")?, "untrusted-archive");
        let temp = tempfile::tempdir()?;
        let environment = Environment {
            variables: BTreeMap::new(),
            rust_toolchain: "1.95.0".into(),
            tools: BTreeMap::from([(
                "cargo".into(),
                ToolIdentity {
                    executable: FileIdentity::record(&std::env::current_exe()?)?,
                    version: "command fixture".into(),
                },
            )]),
        };
        let settings = settings(&environment);
        let requested = [("codex-app-server", "codex-app-server")];
        let ordinary = cargo_build_command(
            temp.path(),
            &temp.path().join("target"),
            &environment,
            &requested,
            &settings,
            None,
        );
        let overrides: BTreeMap<_, _> = ordinary.get_envs().collect();
        for key in [
            "RUSTY_V8_ARCHIVE",
            "RUSTY_V8_SRC_BINDING_PATH",
            "RUSTY_V8_MIRROR",
            "V8_FROM_SOURCE",
            "GN_ARGS",
        ] {
            assert_eq!(
                overrides.get(OsStr::new(key)),
                Some(&None),
                "actual Cargo construction must remove inherited {key}"
            );
        }
        let mut verified = settings;
        verified.insert("RUSTY_V8_ARCHIVE".into(), "verified-archive".into());
        verified.insert(
            "RUSTY_V8_SRC_BINDING_PATH".into(),
            "verified-binding".into(),
        );
        let native = cargo_build_command(
            temp.path(),
            &temp.path().join("target"),
            &environment,
            &requested,
            &verified,
            Some("x86_64-pc-windows-msvc"),
        );
        let overrides: BTreeMap<_, _> = native.get_envs().collect();
        assert_eq!(
            overrides.get(OsStr::new("RUSTY_V8_ARCHIVE")),
            Some(&Some(OsStr::new("verified-archive")))
        );
        assert_eq!(
            overrides.get(OsStr::new("RUSTY_V8_SRC_BINDING_PATH")),
            Some(&Some(OsStr::new("verified-binding")))
        );
        assert_eq!(overrides.get(OsStr::new("RUSTY_V8_MIRROR")), Some(&None));
        assert_eq!(overrides.get(OsStr::new("CARGO_BUILD_TARGET")), Some(&None));
        let arguments: Vec<_> = native
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--target", "x86_64-pc-windows-msvc"])
        );
        assert!(arguments.windows(2).any(|pair| pair == ["--jobs", "6"]));
        assert!(arguments.contains(&"--locked".into()));
        assert!(arguments.contains(&"--release".into()));
        Ok(())
    }
}
