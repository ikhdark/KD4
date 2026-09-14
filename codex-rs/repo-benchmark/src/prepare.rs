pub mod builds;
pub mod environment;
pub mod provenance;
#[cfg(test)]
mod tests;
mod v8;

use crate::schedule::{Mode, ScheduledAttempt, Variant, schedule};
use crate::workloads::{LiveTask, PreparedFixture, prepare_dependencies_with_env, prepare_fixture};
use anyhow::{Context, Result, ensure};
use environment::{BASE_CONFIG, Environment};
use provenance::{
    FileIdentity, command_output, copy_tree, find_repo_root, git, hash_bytes, hash_tree,
    materialize_commit, read_json, reset_workspace, write_json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub const MANIFEST_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub struct PrepareOptions {
    pub repo: PathBuf,
    pub mode: Mode,
    pub fork_ref: String,
    pub reference_checkout: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceIdentity {
    pub origin: PathBuf,
    pub selection: String,
    pub revision: String,
    pub tree: String,
    pub checkout: PathBuf,
    pub upstream: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrozenFixture {
    pub snapshot: PathBuf,
    pub sha256: String,
    pub descriptor: Option<PreparedFixture>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prepared {
    pub schema_version: u32,
    pub id: String,
    pub directory: PathBuf,
    pub repo: PathBuf,
    pub mode: Mode,
    pub schedule: Vec<ScheduledAttempt>,
    pub workspace: PathBuf,
    pub additional_roots: Vec<PathBuf>,
    pub runs_directory: PathBuf,
    pub import_directory: PathBuf,
    pub fork: SourceIdentity,
    pub reference: SourceIdentity,
    pub builds: BTreeMap<Variant, builds::BuildIdentity>,
    pub harness: FileIdentity,
    pub harness_sources: FileIdentity,
    pub environment: Environment,
    pub base_config: FileIdentity,
    pub features: Vec<Value>,
    pub feature_inventory: FileIdentity,
    pub overrides: BTreeMap<Variant, Vec<String>>,
    pub fixtures: BTreeMap<String, FrozenFixture>,
    pub shared_inputs: PathBuf,
    pub shared_sha256: String,
    pub analyzer: FileIdentity,
    pub analyzer_files: Vec<FileIdentity>,
    pub preparation_ms: u64,
    pub budgets: Value,
}

pub fn unique_id() -> String {
    format!(
        "{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id()
    )
}

pub fn resolve_source(
    origin: &Path,
    selection: &str,
    checkout: PathBuf,
    upstream: bool,
) -> Result<SourceIdentity> {
    let origin = fs::canonicalize(origin)?;
    let revision = git(
        &origin,
        &["rev-parse", "--verify", &format!("{selection}^{{commit}}")],
    )
    .with_context(|| {
        format!(
            "cannot resolve selected revision {selection} in {} (no automatic fetch)",
            origin.display()
        )
    })?;
    let tree = git(&origin, &["rev-parse", &format!("{revision}^{{tree}}")])?;
    Ok(SourceIdentity {
        origin,
        selection: selection.into(),
        revision,
        tree,
        checkout,
        upstream,
    })
}

fn checkout(source: &SourceIdentity) -> Result<()> {
    command_output(
        Command::new("git")
            .args(["-c", "core.longpaths=true"])
            .arg("-C")
            .arg(&source.origin)
            .args(["worktree", "add", "--detach"])
            .arg(provenance::git_path(&source.checkout))
            .arg(&source.revision),
    )?;
    ensure!(
        git(&source.checkout, &["rev-parse", "HEAD"])? == source.revision,
        "wrong prepared native revision"
    );
    Ok(())
}

pub fn feature_overrides(features: &[Value], enabled: bool) -> Result<Vec<String>> {
    let mut values = BTreeMap::new();
    for feature in features {
        let kind = feature
            .pointer("/benchmark_control/kind")
            .and_then(Value::as_str)
            .context("feature lacks benchmark control classification")?;
        ensure!(
            kind != "build",
            "feature {} requires compile-time ablation settings not present in its inventory; cannot share a fork binary",
            feature["id"]
        );
        if kind != "runtime" {
            continue;
        }
        let on = feature
            .get("benchmark_on")
            .and_then(Value::as_bool)
            .context("runtime feature lacks intended benchmark_on status")?;
        for key in feature["config_keys"]
            .as_array()
            .context("feature config keys")?
        {
            let key = key.as_str().context("feature config key")?;
            ensure!(
                key.starts_with("features."),
                "unsupported non-feature boolean control {key}"
            );
            let value = enabled && on;
            if let Some(previous) = values.insert(key.to_owned(), value) {
                ensure!(
                    previous == value,
                    "conflicting inventory settings for {key}"
                );
            }
        }
    }
    ensure!(
        values.contains_key("features.kd4_runtime"),
        "feature inventory lacks KD4 runtime ablation"
    );
    Ok(values
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect())
}

fn freeze_harness_sources(repo: &Path, destination: &Path) -> Result<FileIdentity> {
    let committed_tree = git(repo, &["rev-parse", "HEAD^{tree}"])?;
    let diff = command_output(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["diff", "HEAD", "--binary"]),
    )?;
    // Include untracked harness files; their identity cannot be represented by HEAD or git diff.
    let package_hash = hash_tree(&repo.join("codex-rs/repo-benchmark"))?;
    let build_path = repo.join("codex-rs/target/repo-benchmark/harness/build-provenance.json");
    let build: Value = serde_json::from_slice(
        &fs::read(&build_path)
            .context("harness build provenance missing; prepare through just repo-benchmark")?,
    )?;
    ensure!(
        build["sha256"].as_str()
            == Some(provenance::hash_file(&std::env::current_exe()?)?.as_str()),
        "running harness differs from its Cargo build provenance; rebuild through just repo-benchmark"
    );
    let cargo_inputs = [
        "codex-rs/Cargo.toml",
        "codex-rs/Cargo.lock",
        "codex-rs/.cargo/config.toml",
        "codex-rs/rust-toolchain.toml",
    ]
    .into_iter()
    .map(|p| FileIdentity::record(&repo.join(p)))
    .collect::<Result<Vec<_>>>()?;
    write_json(
        destination,
        &serde_json::json!({"committedTree":committed_tree,"workingDiffSha256":hash_bytes(&diff),"packageSha256":package_hash,"dirty":!diff.is_empty(),"build":build,"cargoInputs":cargo_inputs}),
    )?;
    fs::write(destination.with_extension("patch"), diff)?;
    FileIdentity::record(destination)
}

fn snapshot_shared_inputs(repo: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    copy_tree(&repo.join("scripts"), &destination.join("scripts"))?;
    for name in [
        "source_owners.toml",
        "architecture_index.json",
        "SOURCEMAP.md",
        "kd4_features.toml",
        "justfile",
    ] {
        let input = repo.join(name);
        if input.is_file() {
            fs::copy(input, destination.join(name))?;
        }
    }
    // Shared helpers must not reintroduce nested instructions or project config
    // after the committed task tree has been stripped. Restore only the approved
    // root instruction bytes after removing inherited instruction sources.
    strip_inherited_configuration(destination)?;
    fs::copy(repo.join("AGENTS.md"), destination.join("AGENTS.md"))?;
    Ok(())
}

fn strip_inherited_configuration(workspace: &Path) -> Result<()> {
    let entries = walkdir::WalkDir::new(workspace)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for entry in entries {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        let config = entry
            .path()
            .parent()
            .is_some_and(|p| p.file_name().is_some_and(|n| n == ".codex"))
            && name == "config.toml";
        if name == "AGENTS.md" || name == "AGENTS.override.md" || config {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

pub fn prepare(options: PrepareOptions) -> Result<PathBuf> {
    let started = Instant::now();
    let repo = find_repo_root(&options.repo)?;
    let artifact_root = repo.join("codex-rs/target/repo-benchmark");
    let id = unique_id();
    let directory = artifact_root.join("prepared").join(&id);
    fs::create_dir_all(directory.join("sources"))?;
    let directory = fs::canonicalize(directory)?;
    let workspace = directory.join("workspace");
    let fork = resolve_source(
        &repo,
        &options.fork_ref,
        directory.join("sources/fork"),
        false,
    )?;
    let reference = match &options.reference_checkout {
        Some(path) => resolve_source(path, "HEAD", directory.join("sources/reference"), false)?,
        None => resolve_source(
            &repo,
            "upstream/main",
            directory.join("sources/reference"),
            true,
        )?,
    };
    let environment = Environment::capture(&repo)?;
    fs::create_dir_all(directory.join("frozen"))?;
    let base_config_path = directory.join("frozen/config.toml");
    fs::write(&base_config_path, BASE_CONFIG)?;
    let inventory_path = directory.join("frozen/kd4_features.toml");
    fs::copy(repo.join("kd4_features.toml"), &inventory_path)?;
    let inventory: toml::Value = toml::from_str(&fs::read_to_string(&inventory_path)?)?;
    let features: Vec<Value> =
        serde_json::from_value(serde_json::to_value(&inventory["features"])?)?;
    let overrides = BTreeMap::from([
        (Variant::ForkOff, feature_overrides(&features, false)?),
        (Variant::ForkOn, feature_overrides(&features, true)?),
        (Variant::Reference, vec![]),
    ]);
    let shared_inputs = directory.join("frozen/shared");
    snapshot_shared_inputs(&repo, &shared_inputs)?;
    let shared_sha256 = hash_tree(&shared_inputs)?;
    let analyzer_root = directory.join("frozen/analyzer");
    fs::create_dir_all(&analyzer_root)?;
    let mut analyzer_files = vec![];
    for name in [
        "kd4_turn_latency_audit.py",
        "kd4_timing_analysis.py",
        "kd4_first_useful_action_analysis.py",
        "rollout_snapshot.py",
    ] {
        let destination = analyzer_root.join(name);
        fs::copy(repo.join("scripts").join(name), &destination)?;
        analyzer_files.push(FileIdentity::record(&destination)?);
    }
    let analyzer = FileIdentity::record(&analyzer_root.join("kd4_turn_latency_audit.py"))?;
    let harness_sources =
        freeze_harness_sources(&repo, &directory.join("frozen/harness-source.json"))?;
    let harness_path = directory.join("frozen").join(if cfg!(windows) {
        "repo-benchmark.exe"
    } else {
        "repo-benchmark"
    });
    fs::copy(std::env::current_exe()?, &harness_path)?;
    let harness = FileIdentity::record(&harness_path)?;
    checkout(&fork)?;
    checkout(&reference)?;
    // A checkout predating the new switch cannot honestly participate in the ablation.
    let fork_features = fs::read_to_string(fork.checkout.join("codex-rs/features/src/lib.rs"))?;
    ensure!(
        fork_features.contains("kd4_runtime"),
        "fork revision {} lacks features.kd4_runtime; select a committed implementation revision and prepare again",
        fork.revision
    );
    let mut fork_targets = vec![
        ("codex-app-server", "codex-app-server"),
        ("codex-cli", "codex"),
    ];
    if fork
        .checkout
        .join("codex-rs/code-mode-host/Cargo.toml")
        .is_file()
    {
        fork_targets.push(("codex-code-mode-host", "codex-code-mode-host"));
    }
    let fork_build = builds::build(
        &fork.checkout,
        &fork.revision,
        &artifact_root.join("builds/fork"),
        &environment,
        &fork_targets,
    )
    .with_context(|| format!("fork variants at {}", fork.revision))?;
    let mut reference_targets = vec![
        ("codex-app-server", "codex-app-server"),
        ("codex-cli", "codex"),
    ];
    if reference
        .checkout
        .join("codex-rs/code-mode-host/Cargo.toml")
        .is_file()
    {
        reference_targets.push(("codex-code-mode-host", "codex-code-mode-host"));
    }
    let reference_build = builds::build(
        &reference.checkout,
        &reference.revision,
        &artifact_root.join("builds/reference"),
        &environment,
        &reference_targets,
    )
    .with_context(|| format!("reference at {}", reference.revision))?;
    let builds = BTreeMap::from([
        (Variant::ForkOff, fork_build.clone()),
        (Variant::ForkOn, fork_build),
        (Variant::Reference, reference_build),
    ]);
    let mut fixtures = BTreeMap::new();
    let scripted = directory.join("fixtures/scripted");
    copy_tree(&shared_inputs, &scripted)?;
    fs::write(
        scripted.join("benchmark-input.txt"),
        "Repo Benchmark deterministic input\n",
    )?;
    fixtures.insert(
        "scripted".into(),
        FrozenFixture {
            sha256: hash_tree(&scripted)?,
            snapshot: scripted,
            descriptor: None,
        },
    );
    let tasks = if options.mode == Mode::Fast {
        vec![LiveTask::RustBugfix]
    } else {
        vec![
            LiveTask::RustBugfix,
            LiveTask::TypescriptFeature,
            LiveTask::Kd4PythonRefactor,
        ]
    };
    for task in tasks {
        let snapshot = directory.join("fixtures").join(task.id());
        fs::create_dir_all(&snapshot)?;
        if task == LiveTask::Kd4PythonRefactor {
            materialize_commit(&fork.origin, &fork.revision, &snapshot)?;
        }
        strip_inherited_configuration(&snapshot)?;
        copy_tree(&shared_inputs, &snapshot)?;
        reset_workspace(&directory, &workspace, &snapshot)?;
        let protected = directory.join("protected").join(task.id());
        fs::create_dir_all(&protected)?;
        let descriptor = prepare_fixture(task, &workspace, &protected)?;
        prepare_dependencies_with_env(&descriptor, &environment.variables)?;
        copy_tree(&workspace, &snapshot)?;
        fixtures.insert(
            task.id().into(),
            FrozenFixture {
                sha256: hash_tree(&snapshot)?,
                snapshot,
                descriptor: Some(descriptor),
            },
        );
    }
    let prepared = Prepared {
        schema_version: MANIFEST_VERSION,
        id,
        directory: directory.clone(),
        repo: repo.clone(),
        mode: options.mode,
        schedule: schedule(options.mode),
        workspace,
        additional_roots: vec![],
        runs_directory: directory.join("runs"),
        import_directory: repo.join("docs/benchmarks/repo-benchmark/accepted"),
        fork,
        reference,
        builds,
        harness,
        harness_sources,
        environment,
        base_config: FileIdentity::record(&base_config_path)?,
        features,
        feature_inventory: FileIdentity::record(&inventory_path)?,
        overrides,
        fixtures,
        shared_inputs,
        shared_sha256,
        analyzer,
        analyzer_files,
        preparation_ms: started.elapsed().as_millis() as u64,
        budgets: serde_json::json!({"scriptedMs":crate::schedule::SCRIPTED_LIMIT_MS,"realModelMs":options.mode.live_limit_ms(),"attemptMs":crate::schedule::ATTEMPT_LIMIT_MS,"verifierMs":120000,"initialFixedCeilings":true}),
    };
    let manifest = directory.join("prepared.json");
    write_json(&manifest, &prepared)?;
    eprintln!("Prepared {}", manifest.display());
    Ok(manifest)
}

impl Prepared {
    pub fn load(path: &Path) -> Result<Self> {
        let prepared: Self = read_json(path).context(
            "new Repo Benchmark manifest required; legacy comparisons must be prepared again",
        )?;
        ensure!(
            prepared.schema_version == MANIFEST_VERSION,
            "unsupported prepared manifest version"
        );
        ensure!(
            prepared.schedule == schedule(prepared.mode),
            "prepared workload schedule changed"
        );
        Ok(prepared)
    }
    pub fn verify(&self) -> Result<()> {
        self.harness.verify()?;
        self.harness_sources.verify()?;
        self.base_config.verify()?;
        self.feature_inventory.verify()?;
        self.environment.verify()?;
        for file in &self.analyzer_files {
            file.verify()?;
        }
        ensure!(
            hash_tree(&self.shared_inputs)? == self.shared_sha256,
            "shared instructions/scripts changed"
        );
        for (name, fixture) in &self.fixtures {
            ensure!(
                hash_tree(&fixture.snapshot)? == fixture.sha256,
                "fixture {name} changed"
            );
            if let Some(descriptor) = &fixture.descriptor {
                ensure!(
                    provenance::hash_file(&descriptor.verifier_path)? == descriptor.verifier_sha256,
                    "verifier for {name} changed"
                );
            }
        }
        for source in [&self.fork, &self.reference] {
            ensure!(
                git(&source.checkout, &["rev-parse", "HEAD"])? == source.revision,
                "native checkout revision changed"
            );
            ensure!(
                git(
                    &source.checkout,
                    &["status", "--porcelain", "--untracked-files=no"]
                )?
                .is_empty(),
                "native source at {} is modified",
                source.revision
            );
        }
        for build in self.builds.values() {
            build.verify()?;
        }
        Ok(())
    }
    pub fn expected_config(&self, variant: Variant) -> Result<Value> {
        let mut config: toml::Value = toml::from_str(&fs::read_to_string(&self.base_config.path)?)?;
        for setting in &self.overrides[&variant] {
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
}
