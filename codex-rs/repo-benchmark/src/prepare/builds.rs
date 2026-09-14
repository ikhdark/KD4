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

pub fn build(
    source: &Path,
    revision: &str,
    target_root: &Path,
    env: &Environment,
    requested: &[(&str, &str)],
) -> Result<BuildIdentity> {
    let started = Instant::now();
    let workspace = source.join("codex-rs");
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
        return Ok(previous);
    }
    let log = target_directory.join("build.log");
    let mut log_file = fs::File::create(&log)?;
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
    if let Some(artifacts) = &v8_artifacts {
        command.env_remove("CARGO_BUILD_TARGET");
        command.args(["--target", &artifacts.target]);
    }
    for (key, value) in &settings {
        if key != "jobs" && key != "profile" {
            command.env(key, value);
        }
    }
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
