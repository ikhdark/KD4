//! Live fixtures and independent verification; execution timing belongs to the runner.
mod fixtures;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveTask {
    RustBugfix,
    TypescriptFeature,
    Kd4PythonRefactor,
}

impl LiveTask {
    pub fn id(self) -> &'static str {
        match self {
            Self::RustBugfix => "rust_bugfix",
            Self::TypescriptFeature => "typescript_feature",
            Self::Kd4PythonRefactor => "kd4_python_refactor",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedFixture {
    pub task: LiveTask,
    pub workspace: PathBuf,
    pub protected_dir: PathBuf,
    pub prompt: String,
    pub verifier_path: PathBuf,
    pub verifier_sha256: String,
    pub initial_source_hashes: BTreeMap<String, String>,
    pub initial_test_hashes: BTreeMap<String, String>,
    pub initial_workspace_hashes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Passed,
    Incorrect,
    ScopeViolation,
    Unavailable,
    TimedOut,
    IntegrityFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationOutcome {
    pub status: VerificationStatus,
    pub elapsed_ms: u64,
    pub detail: String,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

pub(crate) fn hash_file(path: &Path) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(fs::read(path).with_context(|| format!("read {}", path.display()))?)
    ))
}

fn workspace_hashes(
    root: &Path,
    directory: &Path,
    hashes: &mut BTreeMap<String, String>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!(
                "fixture contains an unsupported symbolic link: {}",
                path.display()
            );
        }
        if kind.is_dir() {
            if ![
                ".git",
                "target",
                "node_modules",
                "__pycache__",
                ".pytest_cache",
            ]
            .contains(&entry.file_name().to_string_lossy().as_ref())
            {
                workspace_hashes(root, &path, hashes)?;
            }
        } else if kind.is_file() {
            hashes.insert(
                path.strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/"),
                hash_file(&path)?,
            );
        }
    }
    Ok(())
}

fn configure_helper(command: &mut Command, environment: Option<&BTreeMap<String, String>>) {
    if let Some(environment) = environment {
        command.env_clear().envs(environment);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
}

/// The caller supplies the pinned Git tree and common instructions/scripts first.
/// Both roots must already exist, and the protected root must be outside the task tree.
pub fn prepare_fixture(
    task: LiveTask,
    workspace: &Path,
    protected: &Path,
) -> Result<PreparedFixture> {
    let workspace = workspace.canonicalize()?;
    let protected = protected.canonicalize()?;
    if protected.starts_with(&workspace) || workspace.starts_with(&protected) {
        bail!("task workspace and verifier directory must be disjoint");
    }
    let spec = fixtures::install(task, &workspace)?;
    let verifier_path = protected.join("verify.py");
    fs::write(&verifier_path, include_str!("workloads/verify.py"))?;
    let source_hashes = spec
        .sources
        .iter()
        .map(|p| Ok((p.to_string(), hash_file(&workspace.join(p))?)))
        .collect::<Result<_>>()?;
    let test_hashes = spec
        .tests
        .iter()
        .map(|p| Ok((p.to_string(), hash_file(&workspace.join(p))?)))
        .collect::<Result<_>>()?;
    for path in spec.tests {
        let destination = protected.join("baseline_tests").join(path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(workspace.join(path), destination)?;
    }
    let mut initial_workspace_hashes = BTreeMap::new();
    workspace_hashes(&workspace, &workspace, &mut initial_workspace_hashes)?;
    let fixture = PreparedFixture {
        task,
        workspace,
        protected_dir: protected,
        prompt: spec.prompt.into(),
        verifier_sha256: hash_file(&verifier_path)?,
        verifier_path,
        initial_source_hashes: source_hashes,
        initial_test_hashes: test_hashes,
        initial_workspace_hashes,
    };
    fs::write(
        fixture.protected_dir.join("fixture.json"),
        serde_json::to_vec_pretty(&fixture)?,
    )?;
    Ok(fixture)
}

/// Prepare dependency-free task projects outside benchmark execution timing.
pub fn prepare_dependencies(fixture: &PreparedFixture) -> Result<()> {
    prepare_dependencies_inner(fixture, None)
}

pub fn prepare_dependencies_with_env(
    fixture: &PreparedFixture,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    prepare_dependencies_inner(fixture, Some(environment))
}

fn prepare_dependencies_inner(
    fixture: &PreparedFixture,
    environment: Option<&BTreeMap<String, String>>,
) -> Result<()> {
    let mut command = if fixture.task == LiveTask::RustBugfix {
        let mut c = Command::new("cargo");
        c.args(["test", "--offline", "--locked", "--no-run", "--jobs", "6"]);
        c
    } else if fixture.task == LiveTask::TypescriptFeature {
        let mut c = Command::new("node");
        c.args(["--experimental-strip-types", "--input-type=module", "-e", "import('./src/report.ts').then(m => { if (m.renderReport('apples,2') !== 'apples: 2\\nTOTAL: 2') process.exit(1); })"]);
        c
    } else {
        let mut c = Command::new("python");
        c.args(["-I", "-c", "import sys; assert sys.version_info >= (3, 11)"]);
        c
    };
    configure_helper(&mut command, environment);
    command
        .current_dir(&fixture.workspace)
        .env_remove("CARGO_TARGET_DIR");
    let status = command.status().context("prepare task dependencies")?;
    if !status.success() {
        bail!(
            "dependency preparation failed for {}: {status}",
            fixture.task.id()
        );
    }
    Ok(())
}

pub fn verify_fixture(fixture: &PreparedFixture) -> Result<VerificationOutcome> {
    verify_fixture_inner(fixture, None)
}

pub fn verify_fixture_with_env(
    fixture: &PreparedFixture,
    environment: &BTreeMap<String, String>,
) -> Result<VerificationOutcome> {
    verify_fixture_inner(fixture, Some(environment))
}

fn verify_fixture_inner(
    fixture: &PreparedFixture,
    environment: Option<&BTreeMap<String, String>>,
) -> Result<VerificationOutcome> {
    let started = Instant::now();
    let stdout_path = fixture.protected_dir.join("verification.stdout.log");
    let stderr_path = fixture.protected_dir.join("verification.stderr.log");
    let outcome = |status, detail| VerificationOutcome {
        status,
        detail,
        elapsed_ms: started.elapsed().as_millis() as u64,
        stdout_path: stdout_path.clone(),
        stderr_path: stderr_path.clone(),
    };
    let valid = hash_file(&fixture.verifier_path).is_ok_and(|hash| hash == fixture.verifier_sha256);
    if !valid {
        return Ok(outcome(
            VerificationStatus::IntegrityFailure,
            "protected verifier changed or disappeared".into(),
        ));
    }
    for (name, hash) in &fixture.initial_test_hashes {
        if !hash_file(&fixture.protected_dir.join("baseline_tests").join(name))
            .is_ok_and(|actual| actual == *hash)
        {
            return Ok(outcome(
                VerificationStatus::IntegrityFailure,
                format!("protected original test changed: {name}"),
            ));
        }
    }
    // Rewrite only from the in-memory, manifest-bound descriptor; ignore any task-produced descriptor.
    fs::write(
        fixture.protected_dir.join("fixture.json"),
        serde_json::to_vec_pretty(fixture)?,
    )?;
    let mut command = Command::new("python");
    configure_helper(&mut command, environment);
    let mut child = match command
        .arg("-I")
        .arg(&fixture.verifier_path)
        .arg(fixture.protected_dir.join("fixture.json"))
        .current_dir(&fixture.protected_dir)
        .env_remove("PYTHONPATH")
        .stdout(Stdio::from(fs::File::create(&stdout_path)?))
        .stderr(Stdio::from(fs::File::create(&stderr_path)?))
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return Ok(outcome(
                VerificationStatus::Unavailable,
                format!("cannot launch verifier: {error}"),
            ));
        }
    };
    loop {
        if let Some(status) = child.try_wait()? {
            if !hash_file(&fixture.verifier_path).is_ok_and(|hash| hash == fixture.verifier_sha256)
                || fixture.initial_test_hashes.iter().any(|(name, expected)| {
                    !hash_file(&fixture.protected_dir.join("baseline_tests").join(name))
                        .is_ok_and(|actual| actual == *expected)
                })
            {
                return Ok(outcome(
                    VerificationStatus::IntegrityFailure,
                    "protected verifier or original tests changed during execution".into(),
                ));
            }
            let result = match status.code() {
                Some(0) => VerificationStatus::Passed,
                Some(1) => VerificationStatus::Incorrect,
                Some(3) => VerificationStatus::TimedOut,
                Some(4) => VerificationStatus::ScopeViolation,
                _ => VerificationStatus::Unavailable,
            };
            let detail = fs::read_to_string(&stdout_path).unwrap_or_default();
            return Ok(outcome(result, detail));
        }
        if started.elapsed() >= Duration::from_secs(120) {
            #[cfg(windows)]
            {
                let mut cleanup = Command::new("taskkill");
                configure_helper(&mut cleanup, environment);
                let _ = cleanup
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.kill();
            let _ = child.wait();
            return Ok(outcome(
                VerificationStatus::TimedOut,
                "independent verification exceeded 120 seconds".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
