pub mod builds;
pub mod environment;
pub mod provenance;
#[cfg(test)]
mod tests;
mod v8;

use crate::schedule::Mode;
use crate::schedule::ScheduledAttempt;
use crate::schedule::Variant;
use crate::schedule::schedule;
use crate::workloads::LiveTask;
use crate::workloads::PreparedFixture;
use crate::workloads::prepare_dependencies_with_env;
use crate::workloads::prepare_fixture;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use environment::BASE_CONFIG;
use environment::Environment;
use environment::ProjectConfigComparison;
use environment::configured_value;
use provenance::FileIdentity;
use provenance::command_output;
use provenance::copy_tree;
use provenance::find_repo_root;
use provenance::git;
use provenance::hash_bytes;
use provenance::hash_tree;
use provenance::materialize_commit;
use provenance::read_json;
use provenance::reset_workspace;
use provenance::write_json;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub const MANIFEST_VERSION: u32 = 3;

#[derive(Clone, Debug)]
pub struct PrepareOptions {
    pub repo: PathBuf,
    pub mode: Mode,
    pub fork_ref: String,
    pub reference_checkout: Option<PathBuf>,
}

/// Upstream release tags bump `[workspace.package].version` without
/// regenerating `Cargo.lock`, so the committed lock records its own members at
/// their pre-release version rather than the version being built.
/// Offline resolution repairs exactly those member entries; this records the
/// repair so the changed working tree stays accountable.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockReconciliation {
    pub path: PathBuf,
    pub committed_sha256: String,
    pub resolved_sha256: String,
    /// Workspace members whose recorded version changed. No registry entry moved.
    pub members: Vec<String>,
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
    /// Absent when the committed lockfile already matched its own manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_reconciliation: Option<LockReconciliation>,
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
    pub workspace_lock: PathBuf,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_config_comparison: Option<ProjectConfigComparison>,
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
        lock_reconciliation: None,
    })
}

/// The one lockfile in a native checkout that Cargo resolves for these builds.
const WORKSPACE_LOCK: &str = "codex-rs/Cargo.lock";

/// True when a reconciled checkout's `git status --porcelain` reports nothing
/// but the workspace lockfile. Porcelain v1 prints two status characters, a
/// space, then the path, but `git` trims its output and an unstaged change
/// leads with a space, so take the path after the status field rather than at a
/// fixed offset. A line without a status field is never accepted.
fn only_workspace_lock_modified(status: &str) -> bool {
    status.lines().all(|line| {
        line.trim_start()
            .split_once(' ')
            .is_some_and(|(_, path)| path.trim() == WORKSPACE_LOCK)
    })
}

/// Accept only the upstream release-tag difference: the locked package list must
/// keep its exact order and membership, every registry entry must stay
/// byte-identical, and a local member may differ in nothing but its version.
fn workspace_version_only_changes(committed: &str, resolved: &str) -> Result<Vec<String>> {
    let split = |text: &str| -> Result<(Vec<toml::Value>, toml::Value)> {
        let mut document: toml::Value = toml::from_str(text)?;
        let packages = document
            .get("package")
            .and_then(toml::Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some(table) = document.as_table_mut() {
            table.remove("package");
        }
        Ok((packages, document))
    };
    let (committed, committed_rest) = split(committed)?;
    let (resolved, resolved_rest) = split(resolved)?;
    ensure!(
        committed_rest == resolved_rest,
        "offline resolution changed lockfile metadata outside the package list"
    );
    ensure!(
        committed.len() == resolved.len(),
        "offline resolution changed the locked package set"
    );
    let mut members = Vec::new();
    for (committed, resolved) in committed.iter().zip(resolved.iter()) {
        if committed == resolved {
            continue;
        }
        let name = committed
            .get("name")
            .and_then(toml::Value::as_str)
            .context("locked package name")?;
        ensure!(
            resolved.get("name").and_then(toml::Value::as_str) == Some(name),
            "offline resolution reordered the locked package set at {name}"
        );
        ensure!(
            committed.get("source").is_none() && resolved.get("source").is_none(),
            "offline resolution changed registry package {name}"
        );
        let mut stripped = (committed.clone(), resolved.clone());
        for entry in [&mut stripped.0, &mut stripped.1] {
            if let Some(table) = entry.as_table_mut() {
                table.remove("version");
            }
        }
        ensure!(
            stripped.0 == stripped.1,
            "offline resolution changed locked package {name} beyond its workspace version"
        );
        members.push(name.to_owned());
    }
    ensure!(
        !members.is_empty(),
        "lockfile changed without any workspace member version difference"
    );
    Ok(members)
}

/// Resolve the checkout's own committed lockfile offline, so no registry update
/// can reach it, and accept the result only when it is the release-tag repair.
/// Resolution is scoped to the packages this variant builds: a whole-workspace
/// resolve spans every platform and would demand crates this host never caches.
fn reconcile_lock(
    source: &mut SourceIdentity,
    environment: &Environment,
    requested: &[(&str, &str)],
) -> Result<()> {
    let workspace = source.checkout.join("codex-rs");
    let path = workspace.join("Cargo.lock");
    let committed = fs::read_to_string(&path)?;
    let mut command = Command::new(
        &environment
            .tools
            .get("cargo")
            .context("pinned Cargo identity")?
            .executable
            .path,
    );
    command
        .current_dir(&workspace)
        .args([
            "tree",
            "--offline",
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .env("RUSTUP_TOOLCHAIN", &environment.rust_toolchain)
        .env_remove("CARGO_TARGET_DIR")
        // The resolved graph is read back from the lockfile itself.
        .stdout(Stdio::null());
    // Several requested binaries can belong to one package; keep arguments sorted and unique.
    let packages = BTreeSet::from_iter(requested.iter().map(|(package, _)| *package));
    for package in packages {
        command.args(["-p", package]);
    }
    v8::clear_inherited_overrides(&mut command);
    command_output(&mut command).with_context(|| {
        format!(
            "resolve the committed lockfile of {} offline",
            source.revision
        )
    })?;
    let resolved = fs::read_to_string(&path)?;
    if resolved == committed {
        return Ok(());
    }
    let members = workspace_version_only_changes(&committed, &resolved)
        .with_context(|| format!("reconcile the lockfile of {}", source.revision))?;
    eprintln!(
        "Reconciled {} workspace member versions in the lockfile of {}",
        members.len(),
        source.revision
    );
    source.lock_reconciliation = Some(LockReconciliation {
        committed_sha256: hash_bytes(committed.as_bytes()),
        resolved_sha256: provenance::hash_file(&path)?,
        path,
        members,
    });
    Ok(())
}

fn resolve_reference(
    repo: &Path,
    reference_checkout: Option<&Path>,
    checkout: PathBuf,
) -> Result<SourceIdentity> {
    if let Some(origin) = reference_checkout {
        return resolve_source(origin, "HEAD", checkout, false);
    }
    // Release branches need not be ancestors of upstream/main. Use local release
    // tags so ordinary main commits cannot advance the benchmark baseline.
    let tags = git(repo, &["tag", "--list", "rust-v*"])?;
    let release = tags
        .lines()
        .filter_map(|tag| {
            let mut parts = tag.strip_prefix("rust-v")?.split('.');
            let mut version = [0_u64; 3];
            for component in &mut version {
                let part = parts.next()?;
                if part.is_empty()
                    || !part.bytes().all(|byte| byte.is_ascii_digit())
                    || part.len() > 1 && part.starts_with('0')
                {
                    return None;
                }
                *component = part.parse().ok()?;
            }
            parts.next().is_none().then_some((version, tag))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, tag)| tag)
        .context("no local stable upstream release tag (rust-vMAJOR.MINOR.PATCH); make upstream release tags available or select --reference CHECKOUT (no automatic fetch)")?;
    resolve_source(repo, &format!("refs/tags/{release}"), checkout, true)
}

fn checkout(source: &SourceIdentity) -> Result<()> {
    ensure!(
        source.checkout.is_absolute(),
        "prepared native checkout must be absolute"
    );
    let parent = source.checkout.parent().context("native checkout parent")?;
    fs::create_dir_all(parent)?;
    // Exclusive creation establishes ownership; never clean an existing destination.
    fs::create_dir(&source.checkout)
        .context("native checkout destination already exists or cannot be created")?;
    let owned_path = fs::canonicalize(&source.checkout)?;
    let result = (|| -> Result<()> {
        command_output(
            Command::new("git")
                .args([
                    "-c",
                    "core.longpaths=true",
                    "clone",
                    "--local",
                    "--no-hardlinks",
                    "--no-checkout",
                    "--no-tags",
                    "--config",
                    "core.longpaths=true",
                    "--",
                ])
                .arg(provenance::git_path(&source.origin))
                .arg(provenance::git_path(&source.checkout)),
        )?;
        git(
            &source.checkout,
            &["checkout", "--detach", &source.revision],
        )?;
        ensure!(
            git(&source.checkout, &["rev-parse", "HEAD"])? == source.revision,
            "wrong prepared native revision"
        );
        ensure!(
            git(&source.checkout, &["rev-parse", "HEAD^{tree}"])? == source.tree,
            "wrong prepared native tree"
        );
        Ok(())
    })();
    if let Err(error) = result {
        ensure!(
            fs::canonicalize(&source.checkout)? == owned_path,
            "failed native checkout changed location; retained evidence: {error:#}"
        );
        fs::remove_dir_all(&owned_path).with_context(|| {
            format!(
                "clean newly created failed checkout {} after {error:#}",
                owned_path.display()
            )
        })?;
        return Err(error).context("initialize independent local native checkout");
    }
    Ok(())
}

fn validate_selected_toolchain(
    source: &SourceIdentity,
    expected: &str,
    variant: &str,
) -> Result<()> {
    let declaration = git(
        &source.origin,
        &[
            "show",
            &format!("{}:codex-rs/rust-toolchain.toml", source.revision),
        ],
    )
    .with_context(|| {
        format!(
            "{variant} at {} lacks a committed Rust toolchain declaration",
            source.revision
        )
    })?;
    builds::validate_toolchain(&declaration, &source.revision, expected)
        .with_context(|| format!("{variant} at {}", source.revision))
}

fn snapshot_feature_inventory(source: &SourceIdentity, destination: &Path) -> Result<()> {
    let bytes = command_output(
        Command::new("git")
            .arg("-C")
            .arg(&source.origin)
            .args(["show", &format!("{}:kd4_features.toml", source.revision)]),
    )?;
    fs::write(destination, bytes)?;
    Ok(())
}

pub fn feature_overrides(features: &[Value]) -> Result<Vec<String>> {
    let mut values = BTreeMap::new();
    for feature in features {
        let kind = feature
            .pointer("/benchmark_control/kind")
            .and_then(Value::as_str)
            .context("feature lacks benchmark control classification")?;
        ensure!(
            kind != "build",
            "feature {} requires compile-time settings not present in its inventory",
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
            let value = on;
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
        "feature inventory lacks KD4 runtime control"
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
    for name in ["kd4_features.toml", "justfile"] {
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
    let reference = resolve_reference(
        &repo,
        options.reference_checkout.as_deref(),
        directory.join("sources/reference"),
    )?;
    eprintln!(
        "Reference: {} at {}",
        reference.selection, reference.revision
    );
    let environment = Environment::capture(&repo)?;
    validate_selected_toolchain(&fork, &environment.rust_toolchain, "fork")?;
    validate_selected_toolchain(&reference, &environment.rust_toolchain, "reference")?;
    fs::create_dir_all(directory.join("frozen"))?;
    let base_config_path = directory.join("frozen/config.toml");
    fs::write(&base_config_path, BASE_CONFIG)?;
    let inventory_path = directory.join("frozen/kd4_features.toml");
    snapshot_feature_inventory(&fork, &inventory_path)?;
    let inventory: toml::Value = toml::from_str(&fs::read_to_string(&inventory_path)?)?;
    let features: Vec<Value> =
        serde_json::from_value(serde_json::to_value(&inventory["features"])?)?;
    let overrides = BTreeMap::from([
        (Variant::ForkOn, feature_overrides(&features)?),
        (Variant::Reference, vec![]),
    ]);
    let project_config_comparison =
        ProjectConfigComparison::capture(&repo, BASE_CONFIG, &overrides)?;
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
    let mut fork = fork;
    let mut reference = reference;
    checkout(&fork)?;
    checkout(&reference)?;
    // The fork must support the inventoried runtime controls.
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
    // The build cache key includes the lockfile hash, so reconcile before the
    // build reads this checkout.
    reconcile_lock(&mut fork, &environment, &fork_targets)?;
    let fork_build = builds::build(
        &fork.checkout,
        &fork.revision,
        &artifact_root.join("builds/fork"),
        &environment,
        &fork_targets,
    )
    .with_context(|| format!("fork at {}", fork.revision))?;
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
    reconcile_lock(&mut reference, &environment, &reference_targets)?;
    let reference_build = builds::build(
        &reference.checkout,
        &reference.revision,
        &artifact_root.join("builds/reference"),
        &environment,
        &reference_targets,
    )
    .with_context(|| format!("reference at {}", reference.revision))?;
    let builds = BTreeMap::from([
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
            LiveTask::PythonConsumerRefactor,
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
        reset_workspace(&directory, &workspace, &snapshot, &hash_tree(&snapshot)?)?;
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
        workspace_lock: directory.join("workspace.lock"),
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
        project_config_comparison,
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
            "unsupported prepared manifest version; prepare again"
        );
        ensure!(
            prepared.schedule == schedule(prepared.mode),
            "prepared workload schedule changed"
        );
        // Loaded manifests are an external boundary. Verify mandatory map entries
        // before execution can reset the workspace or publish run evidence.
        for variant in Variant::ALL.into_iter() {
            ensure!(
                prepared.overrides.contains_key(&variant),
                "prepared manifest lacks overrides for {}",
                variant.name()
            );
            let build = prepared
                .builds
                .get(&variant)
                .with_context(|| format!("prepared manifest lacks build for {}", variant.name()))?;
            ensure!(
                build.executables.contains_key("codex-app-server"),
                "prepared manifest lacks codex-app-server executable for {}",
                variant.name()
            );
        }
        for attempt in &prepared.schedule {
            let fixture = match attempt.segment {
                crate::schedule::Segment::Scripted => "scripted",
                crate::schedule::Segment::RealModel => attempt.workload.as_str(),
            };
            ensure!(
                prepared.fixtures.contains_key(fixture),
                "prepared manifest lacks fixture {fixture}"
            );
        }
        ensure!(
            prepared.environment.tools.contains_key("python"),
            "prepared manifest lacks python tool"
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
            let status = git(
                &source.checkout,
                &["status", "--porcelain", "--untracked-files=no"],
            )?;
            match &source.lock_reconciliation {
                None => ensure!(
                    status.is_empty(),
                    "native source at {} is modified",
                    source.revision
                ),
                Some(lock) => {
                    ensure!(
                        only_workspace_lock_modified(&status),
                        "native source at {} is modified beyond its reconciled lockfile",
                        source.revision
                    );
                    ensure!(
                        provenance::hash_file(&lock.path)? == lock.resolved_sha256,
                        "reconciled lockfile at {} changed",
                        source.revision
                    );
                }
            }
        }
        for build in self.builds.values() {
            build.verify()?;
        }
        Ok(())
    }
    pub fn expected_config(&self, variant: Variant) -> Result<Value> {
        configured_value(
            &fs::read_to_string(&self.base_config.path)?,
            &self.overrides[&variant],
        )
    }
}
