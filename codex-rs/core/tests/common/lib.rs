#![allow(clippy::expect_used)]

use anyhow::Context as _;
use anyhow::ensure;
use codex_arg0::Arg0PathEntryGuard;
use codex_utils_cargo_bin::CargoBinError;
use ctor::ctor;
use std::sync::OnceLock;
use tempfile::TempDir;

use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::test_support::CloudConfigBundleFixture;
use codex_core::CodexThread;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
pub use codex_core::test_support::TestCodexResponsesRequestKind;
pub use codex_core::test_support::responses_metadata;
use codex_protocol::models::FileSystemPermissions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
pub use codex_utils_absolute_path::test_support::PathBufExt;
pub use codex_utils_absolute_path::test_support::PathExt;
use codex_utils_path_uri::PathUri;
use regex_lite::Regex;
use std::path::Path;
use std::path::PathBuf;

pub mod apps_test_server;
pub mod context_snapshot;
pub mod hooks;
pub mod process;
pub mod responses;
pub mod streaming_sse;
pub mod test_codex;
pub mod test_codex_exec;
pub mod tracing;

static TEST_ARG0_PATH_ENTRY: OnceLock<Option<Arg0PathEntryGuard>> = OnceLock::new();

#[ctor(unsafe)]
fn enable_deterministic_unified_exec_process_ids_for_tests() {
    codex_core::test_support::set_thread_manager_test_mode(/*enabled*/ true);
    codex_core::test_support::set_deterministic_process_ids(/*enabled*/ true);
}

#[ctor(unsafe)]
fn configure_arg0_dispatch_for_test_binaries() {
    let _ = TEST_ARG0_PATH_ENTRY.get_or_init(codex_arg0::arg0_dispatch);
}

#[ctor(unsafe)]
fn configure_insta_workspace_root_for_snapshot_tests() {
    if std::env::var_os("INSTA_WORKSPACE_ROOT").is_some() {
        return;
    }

    let workspace_root = codex_utils_cargo_bin::repo_root()
        .ok()
        .map(|root| root.join("codex-rs"));

    if let Some(workspace_root) = workspace_root
        && let Ok(workspace_root) = workspace_root.canonicalize()
    {
        // Safety: this ctor runs at process startup before test threads begin.
        unsafe {
            std::env::set_var("INSTA_WORKSPACE_ROOT", workspace_root);
        }
    }
}

#[track_caller]
pub fn assert_regex_match<'s>(pattern: &str, actual: &'s str) -> regex_lite::Captures<'s> {
    let regex = Regex::new(pattern).expect("failed to compile regex");
    regex
        .captures(actual)
        .expect("regex did not match actual value")
}

pub fn test_path_buf_with_windows(unix_path: &str, windows_path: Option<&str>) -> PathBuf {
    if let Some(windows) = windows_path {
        return PathBuf::from(windows);
    }
    let mut path = PathBuf::from(r"C:\");
    path.extend(
        unix_path
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty()),
    );
    path
}

pub fn test_path_buf(unix_path: &str) -> PathBuf {
    test_path_buf_with_windows(unix_path, /*windows_path*/ None)
}

pub fn test_absolute_path_with_windows(
    unix_path: &str,
    windows_path: Option<&str>,
) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(test_path_buf_with_windows(unix_path, windows_path))
        .expect("test path should be absolute")
}

pub fn test_absolute_path(unix_path: &str) -> AbsolutePathBuf {
    test_absolute_path_with_windows(unix_path, /*windows_path*/ None)
}

#[allow(clippy::expect_used)]
pub fn create_directory_symlink(source: &Path, link: &Path) {
    // Running this test locally may require Windows Developer Mode or an elevated process.
    std::os::windows::fs::symlink_dir(source, link)
        .expect("create directory symlink; enable Developer Mode or run the test elevated");
}

pub trait TempDirExt {
    fn abs(&self) -> AbsolutePathBuf;
}

impl TempDirExt for TempDir {
    fn abs(&self) -> AbsolutePathBuf {
        self.path().abs()
    }
}

pub const FIXTURE_VALIDATION_ID: &str = "fixture.validation";
pub const FIXTURE_BASELINE_ID: &str = "fixture.baseline.test";
pub const FIXTURE_TEST_ID: &str = "fixture.test";
pub const FIXTURE_SECOND_VALIDATION_ID: &str = "fixture.validation.infrastructure";
pub const FIXTURE_SECOND_BASELINE_ID: &str = "fixture.baseline.infrastructure-test";
pub const FIXTURE_SECOND_TEST_ID: &str = "fixture.infrastructure-test";

#[derive(Clone, Copy)]
pub enum CanonicalRunnerAttestation {
    Valid,
    Missing,
    MismatchedReportIdentity,
    MismatchedValidationContract,
}

#[derive(Clone, Copy)]
pub enum CanonicalAttemptMode {
    AlwaysPass,
    MissingAttestationFirstThenPass,
    ConfirmedFailureFirstThenPass,
    ConfirmedFailureThenInfrastructureErrorFirstThenPass,
}

/// A dirty repository whose real canonical runner emits a valid, freshly
/// attested completion-proof report. Finalization-surface tests share this
/// fixture so they exercise one accepted-proof contract instead of maintaining
/// protocol-specific look-alike artifact generators.
pub struct AcceptedCompletionProofFixture {
    _repo: TempDir,
    repo_path: PathBuf,
    marker_path: PathBuf,
    canonical_command: String,
}

impl AcceptedCompletionProofFixture {
    pub fn new() -> anyhow::Result<Self> {
        const INVENTORY_HASH: &str =
            "da9a082803ed34c951d3506cb6115511efaaf33dec4800d0d77aef7bdc58bb2e";

        let repo = TempDir::new().context("create accepted completion-proof fixture repository")?;
        let repo_path = repo.path().to_path_buf();
        let marker_root = repo_path.join(".fixture-state");
        std::fs::create_dir_all(&marker_root)?;
        std::fs::create_dir_all(repo_path.join(".codex/validation"))?;
        std::fs::create_dir_all(repo_path.join("src"))?;
        let marker_path = marker_root.join("canonical-command-launched");
        let mutate_restore_marker_path = marker_root.join("mutate-and-restore");
        let focused_failure_path = marker_root.join("focused-validation-fails");
        let python = available_python_command()?;
        let canonical_command = format!("{python} proof.py");
        let focused_command = format!("{python} focused.py {{validation_id}}");

        std::fs::write(repo_path.join(".gitignore"), "/.fixture-state/\n")?;
        std::fs::write(
            repo_path.join(".codex/validation/frozen-test-inventory-v1.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 1,
                "inventory_hash": INVENTORY_HASH,
                "tests": [{
                    "baseline_id": FIXTURE_BASELINE_ID,
                    "framework": "fixture",
                    "ignored": false,
                    "native_id": FIXTURE_BASELINE_ID,
                    "platforms": ["windows"],
                    "source": "src/runtime.rs",
                }],
            }))?,
        )?;
        std::fs::write(
            repo_path.join(".codex/validation/completion-proof.toml"),
            format!(
                r#"schema_version = 2
policy_id = "fixture.completion-proof.v1"
canonical_command = {canonical_command_literal}
focused_command = {focused_command_literal}
frozen_inventory_hash = "{INVENTORY_HASH}"
trusted_bundle_paths = ["focused.py", "proof.py"]
trusted_runner_entrypoints = ["focused.py", "proof.py"]

[[validation]]
id = "{FIXTURE_VALIDATION_ID}"
runner = "rust-gate"
gate = "fixture-gate"
owned_paths = ["src/**"]
consumed_paths = ["src/**"]
timeout_seconds = 30
"#,
                canonical_command_literal = serde_json::to_string(&canonical_command)?,
                focused_command_literal = serde_json::to_string(&focused_command)?,
            ),
        )?;
        std::fs::write(
            repo_path.join(".codex/validation/test-replacements-v1.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 1,
                "frozen_inventory_hash": INVENTORY_HASH,
                "rows": [{
                    "baseline_id": FIXTURE_BASELINE_ID,
                    "resolution": "replacement",
                    "replacement_ids": [FIXTURE_TEST_ID],
                    "preserved_behavior": "the configured validation executes",
                    "product_path": "the real finalization surface",
                    "validation_id": FIXTURE_VALIDATION_ID,
                }],
                "overrides": [],
            }))?,
        )?;
        std::fs::write(
            repo_path.join("proof.py"),
            completion_proof_script(
                &canonical_command,
                &marker_path,
                &mutate_restore_marker_path,
                CanonicalRunnerAttestation::Valid,
                CanonicalAttemptMode::AlwaysPass,
            ),
        )?;
        std::fs::write(
            repo_path.join("focused.py"),
            focused_completion_proof_script(&focused_command, &focused_failure_path),
        )?;
        std::fs::write(
            repo_path.join("src/runtime.rs"),
            "pub const VALUE: u8 = 1;\n",
        )?;

        run_fixture_git(&repo_path, &["init", "--quiet"])?;
        run_fixture_git(&repo_path, &["config", "core.autocrlf", "false"])?;
        run_fixture_git(&repo_path, &["config", "user.name", "KD4 Test"])?;
        run_fixture_git(
            &repo_path,
            &["config", "user.email", "kd4-test@example.invalid"],
        )?;
        run_fixture_git(&repo_path, &["add", "."])?;
        run_fixture_git(&repo_path, &["commit", "--quiet", "-m", "fixture baseline"])?;
        std::fs::write(
            repo_path.join("src/runtime.rs"),
            "pub const VALUE: u8 = 2;\n",
        )?;

        Ok(Self {
            _repo: repo,
            repo_path,
            marker_path,
            canonical_command,
        })
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    pub fn canonical_command(&self) -> &str {
        &self.canonical_command
    }

    pub fn canonical_runner_launched(&self) -> bool {
        self.marker_path.exists()
    }

    pub fn canonical_launch_count(&self) -> anyhow::Result<usize> {
        if !self.marker_path.exists() {
            return Ok(0);
        }
        Ok(std::fs::read_to_string(&self.marker_path)?.lines().count())
    }
}

/// A dirty generic repository whose terminal completion policy names a
/// marker-writing canonical runner. Protocol-boundary tests use this fixture
/// to prove that a blocked terminal result never launches certification.
pub struct BlockedCompletionProofFixture {
    _repo: TempDir,
    _marker_home: TempDir,
    repo_path: PathBuf,
    marker_path: PathBuf,
}

impl BlockedCompletionProofFixture {
    pub fn new() -> anyhow::Result<Self> {
        const INVENTORY_HASH: &str =
            "88fa4db7042495a7a4742134b4347c343db7086313580135b7686a3fb40e5e62";

        let repo = TempDir::new().context("create blocked completion-proof fixture repository")?;
        let marker_home =
            TempDir::new().context("create blocked completion-proof marker directory")?;
        let repo_path = repo.path().to_path_buf();
        let marker_path = marker_home.path().join("canonical-command-launched");
        let python = available_python_command()?;
        let canonical_command = format!("{python} proof.py");
        let focused_command = format!("{python} focused.py {{validation_id}}");

        std::fs::create_dir_all(repo_path.join(".codex/validation"))?;
        std::fs::create_dir_all(repo_path.join("src"))?;
        std::fs::write(
            repo_path.join(".codex/validation/frozen-test-inventory-v1.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 1,
                "inventory_hash": INVENTORY_HASH,
                "tests": [{
                    "baseline_id": "fixture.test",
                    "framework": "fixture",
                    "ignored": false,
                    "native_id": "fixture.test",
                    "platforms": ["windows"],
                    "source": "src/runtime.rs",
                }],
            }))?,
        )?;
        std::fs::write(
            repo_path.join(".codex/validation/completion-proof.toml"),
            format!(
                r#"schema_version = 2
policy_id = "fixture.blocked-completion-proof.v1"
canonical_command = {canonical_command_literal}
focused_command = {focused_command_literal}
frozen_inventory_hash = "{INVENTORY_HASH}"
trusted_bundle_paths = ["focused.py", "proof.py"]
trusted_runner_entrypoints = ["focused.py", "proof.py"]

[[validation]]
id = "fixture.validation"
runner = "protocol-boundary-fixture"
command = ["git", "--version"]
intended_ids = ["fixture.test"]
owned_paths = ["src/**"]
consumed_paths = ["src/**"]
timeout_seconds = 30
"#,
                canonical_command_literal = serde_json::to_string(&canonical_command)?,
                focused_command_literal = serde_json::to_string(&focused_command)?,
            ),
        )?;
        std::fs::write(
            repo_path.join(".codex/validation/test-replacements-v1.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 1,
                "frozen_inventory_hash": INVENTORY_HASH,
                "rows": [{
                    "baseline_id": "fixture.test",
                    "resolution": "replacement",
                    "replacement_ids": ["fixture.test"],
                    "preserved_behavior": "the configured validation executes",
                    "production_path": "the real protocol session path",
                    "validation_id": "fixture.validation",
                }],
                "overrides": [],
            }))?,
        )?;
        std::fs::write(
            repo_path.join("proof.py"),
            format!(
                "from pathlib import Path\nPath({}).write_text('launched', encoding='utf-8')\n",
                serde_json::to_string(&marker_path.to_string_lossy())?
            ),
        )?;
        std::fs::write(repo_path.join("focused.py"), "raise SystemExit(0)\n")?;
        std::fs::write(
            repo_path.join("src/runtime.rs"),
            "pub const VALUE: u8 = 1;\n",
        )?;

        run_fixture_git(&repo_path, &["init", "--quiet"])?;
        run_fixture_git(&repo_path, &["config", "core.autocrlf", "false"])?;
        run_fixture_git(&repo_path, &["config", "user.name", "KD4 Test"])?;
        run_fixture_git(
            &repo_path,
            &["config", "user.email", "kd4-test@example.invalid"],
        )?;
        run_fixture_git(&repo_path, &["add", "."])?;
        run_fixture_git(&repo_path, &["commit", "--quiet", "-m", "fixture baseline"])?;

        std::fs::write(
            repo_path.join("src/runtime.rs"),
            "pub const VALUE: u8 = 2;\n",
        )?;

        Ok(Self {
            _repo: repo,
            _marker_home: marker_home,
            repo_path,
            marker_path,
        })
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    pub fn canonical_runner_launched(&self) -> bool {
        self.marker_path.exists()
    }
}

fn available_python_command() -> anyhow::Result<&'static str> {
    for candidate in if cfg!(windows) {
        ["python", "python3"]
    } else {
        ["python3", "python"]
    } {
        if std::process::Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return Ok(candidate);
        }
    }
    anyhow::bail!("completion-proof protocol fixture requires Python")
}

fn run_fixture_git(repo: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

pub fn completion_proof_script(
    canonical_command: &str,
    marker_path: &Path,
    mutate_restore_marker_path: &Path,
    attestation: CanonicalRunnerAttestation,
    attempt_mode: CanonicalAttemptMode,
) -> String {
    const TEMPLATE: &str = r#"import hashlib
import json
import os
import platform
import shutil
import socket
import subprocess
import sys
import time
import uuid
from pathlib import Path

CANONICAL_COMMAND = __CANONICAL_COMMAND__
MARKER_PATH = Path(__MARKER_PATH__)
MUTATE_RESTORE_MARKER_PATH = Path(__MUTATE_RESTORE_MARKER_PATH__)
VALIDATION_ID = "fixture.validation"
TEST_ID = "fixture.test"
SECOND_VALIDATION_ID = "fixture.validation.infrastructure"
SECOND_TEST_ID = "fixture.infrastructure-test"
ATTEST_RUNNER = __ATTEST_RUNNER__
REPORTED_PID_OFFSET = __REPORTED_PID_OFFSET__
REPORTED_VALIDATION_RUNNER = __REPORTED_VALIDATION_RUNNER__
ATTEMPT_MODE = __ATTEMPT_MODE__

sha256 = lambda value: hashlib.sha256(value).hexdigest()
canonical_json = lambda value: json.dumps(
    value, sort_keys=True, separators=(",", ":")
).encode("utf-8")

def start_identity(requested):
    resolved = str(Path(requested).resolve(strict=True))
    digest = sha256(Path(resolved).read_bytes())
    return {
        "requested": str(requested),
        "resolved_path": resolved,
        "sha256_before": digest,
        "sha256_after": digest,
    }

def finish_identity(identity):
    identity["sha256_after"] = sha256(Path(identity["resolved_path"]).read_bytes())
    return identity

def current_process_executable():
    if os.name != "nt":
        return sys.executable
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    get_current_process = kernel32.GetCurrentProcess
    get_current_process.argtypes = []
    get_current_process.restype = wintypes.HANDLE
    query_full_process_image_name = kernel32.QueryFullProcessImageNameW
    query_full_process_image_name.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        wintypes.LPWSTR,
        ctypes.POINTER(wintypes.DWORD),
    ]
    query_full_process_image_name.restype = wintypes.BOOL
    image_path = ctypes.create_unicode_buffer(32768)
    image_path_length = wintypes.DWORD(len(image_path))
    if not query_full_process_image_name(
        get_current_process(), 0, image_path, ctypes.byref(image_path_length)
    ):
        raise ctypes.WinError(ctypes.get_last_error())
    return image_path.value

def attest_runner(entrypoint_path):
    if not ATTEST_RUNNER or (
        ATTEMPT_MODE == "missing-attestation-first-then-pass"
        and not MARKER_PATH.exists()
    ):
        return
    payload = canonical_json({
        "schema_version": 1,
        "attempt_id": os.environ["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
        "nonce": os.environ["CODEX_COMPLETION_PROOF_NONCE"],
        "process_id": os.getpid(),
        "entrypoint_path": entrypoint_path,
    }) + b"\n"
    endpoint = os.environ["CODEX_COMPLETION_PROOF_RUNNER_ATTESTATION_ENDPOINT"]
    if os.name == "nt":
        with open(endpoint, "r+b", buffering=0) as channel:
            channel.write(payload)
            response = channel.readline()
    else:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
            channel.connect(endpoint)
            channel.sendall(payload)
            response = channel.makefile("rb", buffering=0).readline()
    if response != b"ok\n":
        raise SystemExit("runner attestation rejected")

runner_started_at = time.time_ns()
runner_executable_identity = start_identity(current_process_executable())
runner_entrypoint_identity = start_identity(__file__)
attest_runner(runner_entrypoint_identity["resolved_path"])
prior_launch_count = 0
if MARKER_PATH.exists():
    prior_launch_count = len(MARKER_PATH.read_text(encoding="utf-8").splitlines())
with MARKER_PATH.open("a", encoding="utf-8") as marker:
    marker.write("launched\n")
mutate_restore_mode = (
    MUTATE_RESTORE_MARKER_PATH.read_text(encoding="utf-8").strip()
    if MUTATE_RESTORE_MARKER_PATH.exists()
    else ""
)
if mutate_restore_mode == "always" or (
    mutate_restore_mode == "after-first" and prior_launch_count >= 1
):
    runtime_path = Path("src/runtime.rs")
    original = runtime_path.read_bytes()
    runtime_path.write_bytes(original + b"\\n// transient canonical mutation\\n")
    time.sleep(0.2)
    runtime_path.write_bytes(original)
    time.sleep(0.2)
def execute_validation(validation_id, test_id, runner_selector, execution_id, failure):
    requested_child = sys.executable if failure else "git"
    resolved_child = shutil.which(requested_child)
    if not resolved_child:
        raise SystemExit(f"{requested_child} was not resolved")
    launch_target_identity = start_identity(resolved_child)
    launch_target_identity["requested"] = requested_child
    command = (
        [launch_target_identity["resolved_path"], "-c", "raise SystemExit(7)"]
        if failure
        else [launch_target_identity["resolved_path"], "--version"]
    )
    started_at = time.time_ns()
    child = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    child.communicate()
    ended_at = time.time_ns()
    finish_identity(launch_target_identity)
    classification = "confirmed_validation_failure" if failure else "confirmed_pass"
    outcome = "failed" if failure else "passed"
    validation = {
        "id": validation_id,
        "execution_id": execution_id,
        "runner": REPORTED_VALIDATION_RUNNER,
        "runner_selector": runner_selector,
        "evidence_kind": "structured_test",
        "validation_type": None,
        "classification": classification,
        "intended_ids": [test_id],
        "selected_ids": [test_id],
        "executed_ids": [test_id],
        "intended_count": 1,
        "selected_count": 1,
        "executed_count": 1,
        "outcomes": [{"id": test_id, "outcome": outcome}],
        "exit_code": child.returncode,
        "diagnostic": "fixture confirmed validation failure" if failure else "",
        "confirmed_failure_ids": [test_id] if failure else [],
    }
    validation["report_hash"] = sha256(canonical_json(validation))
    child_report = {
        "validation_id": validation_id,
        "execution_id": execution_id,
        "pid": child.pid,
        "executable": launch_target_identity["resolved_path"],
        "launch_target_identity": launch_target_identity,
        "args_hash": sha256(canonical_json(command)),
        "started_at": started_at,
        "ended_at": ended_at,
        "exit_code": child.returncode,
    }
    return validation, child_report

first_attempt = prior_launch_count == 0
primary_failure = first_attempt and ATTEMPT_MODE in {
    "confirmed-failure-first-then-pass",
    "confirmed-failure-then-infrastructure-error-first-then-pass",
}
execution_id = str(uuid.uuid4())
validation, child_report = execute_validation(
    VALIDATION_ID,
    TEST_ID,
    "fixture-gate",
    execution_id,
    primary_failure,
)
validations = [validation]
child_processes = [child_report]
attempt_classification = validation["classification"]
fatal_error = ""

if ATTEMPT_MODE == "confirmed-failure-then-infrastructure-error-first-then-pass":
    second_execution_id = str(uuid.uuid4())
    if first_attempt:
        second_validation = {
            "id": SECOND_VALIDATION_ID,
            "execution_id": second_execution_id,
            "runner": "rust-gate",
            "runner_selector": "fixture-infrastructure-gate",
            "evidence_kind": "structured_test",
            "validation_type": None,
            "classification": "pre_result_error",
            "intended_ids": [SECOND_TEST_ID],
            "selected_ids": [SECOND_TEST_ID],
            "executed_ids": [],
            "intended_count": 1,
            "selected_count": 1,
            "executed_count": 0,
            "outcomes": [],
            "exit_code": None,
            "diagnostic": "fixture runner infrastructure failed before a validation result",
            "confirmed_failure_ids": [],
        }
        second_validation["report_hash"] = sha256(canonical_json(second_validation))
        validations.append(second_validation)
        attempt_classification = "pre_result_error"
        fatal_error = "fixture infrastructure failure after a confirmed validation failure"
    else:
        second_validation, second_child_report = execute_validation(
            SECOND_VALIDATION_ID,
            SECOND_TEST_ID,
            "fixture-infrastructure-gate",
            second_execution_id,
            False,
        )
        validations.append(second_validation)
        child_processes.append(second_child_report)

fingerprint = os.environ["CODEX_COMPLETION_PROOF_START_FINGERPRINT"]
inventory = json.loads(
    Path(".codex/validation/frozen-test-inventory-v1.json").read_text(encoding="utf-8")
)
runner_ended_at = time.time_ns()
finish_identity(runner_executable_identity)
finish_identity(runner_entrypoint_identity)
report = {
    "schema_version": 2,
    "report_type": "CompletionProofAttemptReportV2",
    "policy_id": "fixture.completion-proof.v1",
    "policy_runner_bundle_sha256": os.environ[
        "CODEX_COMPLETION_PROOF_POLICY_RUNNER_BUNDLE_SHA256"
    ],
    "exact_command": CANONICAL_COMMAND,
    "nonce": os.environ["CODEX_COMPLETION_PROOF_NONCE"],
    "attempt_id": os.environ["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
    "parent_pid": int(os.environ["CODEX_COMPLETION_PROOF_PARENT_PID"]),
    "observed_runner_parent_pid": os.getppid(),
    "runner_process_identity": {
        "pid": os.getpid() + REPORTED_PID_OFFSET,
        "parent_pid": os.getppid(),
        "started_at": runner_started_at,
        "ended_at": runner_ended_at,
        "args_hash": sha256(canonical_json([sys.executable, *sys.argv])),
        "executable_identity": runner_executable_identity,
        "entrypoint_identity": runner_entrypoint_identity,
    },
    "repository_root": os.environ["CODEX_COMPLETION_PROOF_REPOSITORY"],
    "host_identity": {
        "hostname": socket.gethostname() or "unknown",
        "system": platform.system() or "unknown",
        "release": platform.release() or "unknown",
        "machine": platform.machine() or "unknown",
    },
    "inventory_hash": inventory["inventory_hash"],
    "start_fingerprint": fingerprint,
    "end_fingerprint": fingerprint,
    "start_mutation_epoch": int(os.environ["CODEX_COMPLETION_PROOF_MUTATION_EPOCH"]),
    "end_mutation_epoch": int(os.environ["CODEX_COMPLETION_PROOF_MUTATION_EPOCH"]),
    "attempt_classification": attempt_classification,
    "validations": validations,
    "child_processes": child_processes,
    "exceptions": [],
    "overrides": [],
    "workspace": {
        "observed_start_fingerprint": fingerprint,
        "observed_end_fingerprint": fingerprint,
    },
    "fatal_error": fatal_error,
}
report["attempt_report_hash"] = sha256(canonical_json(report))
report_path = Path(os.environ["CODEX_COMPLETION_PROOF_REPORT"])
report_path.parent.mkdir(parents=True, exist_ok=True)
report_path.write_text(json.dumps(report), encoding="utf-8")
if primary_failure:
    raise SystemExit(1)
"#;

    TEMPLATE
        .replace(
            "__CANONICAL_COMMAND__",
            &serde_json::to_string(canonical_command).expect("serialize canonical command"),
        )
        .replace(
            "__MARKER_PATH__",
            &serde_json::to_string(&marker_path.to_string_lossy()).expect("serialize marker path"),
        )
        .replace(
            "__MUTATE_RESTORE_MARKER_PATH__",
            &serde_json::to_string(&mutate_restore_marker_path.to_string_lossy())
                .expect("serialize mutate-restore marker path"),
        )
        .replace(
            "__ATTEST_RUNNER__",
            if matches!(attestation, CanonicalRunnerAttestation::Missing) {
                "False"
            } else {
                "True"
            },
        )
        .replace(
            "__REPORTED_PID_OFFSET__",
            if matches!(
                attestation,
                CanonicalRunnerAttestation::MismatchedReportIdentity
            ) {
                "1"
            } else {
                "0"
            },
        )
        .replace(
            "__REPORTED_VALIDATION_RUNNER__",
            if matches!(
                attestation,
                CanonicalRunnerAttestation::MismatchedValidationContract
            ) {
                "\"typed-validation\""
            } else {
                "\"rust-gate\""
            },
        )
        .replace(
            "__ATTEMPT_MODE__",
            &serde_json::to_string(match attempt_mode {
                CanonicalAttemptMode::AlwaysPass => "always-pass",
                CanonicalAttemptMode::MissingAttestationFirstThenPass => {
                    "missing-attestation-first-then-pass"
                }
                CanonicalAttemptMode::ConfirmedFailureFirstThenPass => {
                    "confirmed-failure-first-then-pass"
                }
                CanonicalAttemptMode::ConfirmedFailureThenInfrastructureErrorFirstThenPass => {
                    "confirmed-failure-then-infrastructure-error-first-then-pass"
                }
            })
            .expect("serialize canonical attempt mode"),
        )
}

pub fn focused_completion_proof_script(
    focused_command_template: &str,
    failure_path: &Path,
) -> String {
    const TEMPLATE: &str = r#"import hashlib
import json
import os
import platform
import socket
import subprocess
import sys
import time
import uuid
from pathlib import Path

EXACT_COMMAND = __EXACT_COMMAND__
FAILURE_PATH = Path(__FAILURE_PATH__)
VALIDATION_ID = "fixture.validation"
TEST_ID = "fixture.test"

sha256 = lambda value: hashlib.sha256(value).hexdigest()
canonical_json = lambda value: json.dumps(
    value, sort_keys=True, separators=(",", ":")
).encode("utf-8")

def start_identity(requested):
    resolved = str(Path(requested).resolve(strict=True))
    digest = sha256(Path(resolved).read_bytes())
    return {
        "requested": str(requested),
        "resolved_path": resolved,
        "sha256_before": digest,
        "sha256_after": digest,
    }

def finish_identity(identity):
    identity["sha256_after"] = sha256(Path(identity["resolved_path"]).read_bytes())
    return identity

def current_process_executable():
    if os.name != "nt":
        return sys.executable
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    get_current_process = kernel32.GetCurrentProcess
    get_current_process.argtypes = []
    get_current_process.restype = wintypes.HANDLE
    query_full_process_image_name = kernel32.QueryFullProcessImageNameW
    query_full_process_image_name.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        wintypes.LPWSTR,
        ctypes.POINTER(wintypes.DWORD),
    ]
    query_full_process_image_name.restype = wintypes.BOOL
    image_path = ctypes.create_unicode_buffer(32768)
    image_path_length = wintypes.DWORD(len(image_path))
    if not query_full_process_image_name(
        get_current_process(), 0, image_path, ctypes.byref(image_path_length)
    ):
        raise ctypes.WinError(ctypes.get_last_error())
    return image_path.value

def attest_runner(entrypoint_path):
    payload = canonical_json({
        "schema_version": 1,
        "attempt_id": os.environ["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
        "nonce": os.environ["CODEX_COMPLETION_PROOF_NONCE"],
        "process_id": os.getpid(),
        "entrypoint_path": entrypoint_path,
    }) + b"\n"
    endpoint = os.environ["CODEX_COMPLETION_PROOF_RUNNER_ATTESTATION_ENDPOINT"]
    if os.name == "nt":
        with open(endpoint, "r+b", buffering=0) as channel:
            channel.write(payload)
            response = channel.readline()
    else:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
            channel.connect(endpoint)
            channel.sendall(payload)
            response = channel.makefile("rb", buffering=0).readline()
    if response != b"ok\n":
        raise SystemExit("runner attestation rejected")

runner_started_at = time.time_ns()
runner_executable_identity = start_identity(current_process_executable())
runner_entrypoint_identity = start_identity(__file__)
attest_runner(runner_entrypoint_identity["resolved_path"])
failure = FAILURE_PATH.exists()
execution_id = str(uuid.uuid4())
launch_target_identity = start_identity(sys.executable)
command = [
    launch_target_identity["resolved_path"],
    "-c",
    "raise SystemExit(7)" if failure else "raise SystemExit(0)",
]
started_at = time.time_ns()
child = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
child.communicate()
ended_at = time.time_ns()
finish_identity(launch_target_identity)

fingerprint = os.environ["CODEX_COMPLETION_PROOF_START_FINGERPRINT"]
inventory = json.loads(
    Path(".codex/validation/frozen-test-inventory-v1.json").read_text(encoding="utf-8")
)
classification = "confirmed_validation_failure" if failure else "confirmed_pass"
outcome = "failed" if failure else "passed"
validation = {
    "id": VALIDATION_ID,
    "execution_id": execution_id,
    "runner": "rust-gate",
    "runner_selector": "fixture-gate",
    "evidence_kind": "structured_test",
    "validation_type": None,
    "classification": classification,
    "intended_ids": [TEST_ID],
    "selected_ids": [TEST_ID],
    "executed_ids": [TEST_ID],
    "intended_count": 1,
    "selected_count": 1,
    "executed_count": 1,
    "outcomes": [{"id": TEST_ID, "outcome": outcome}],
    "exit_code": child.returncode,
    "diagnostic": "fixture confirmed validation failure" if failure else "",
    "confirmed_failure_ids": [TEST_ID] if failure else [],
}
validation["report_hash"] = sha256(canonical_json(validation))
runner_ended_at = time.time_ns()
finish_identity(runner_executable_identity)
finish_identity(runner_entrypoint_identity)
report = {
    "schema_version": 2,
    "report_type": "FocusedValidationAttemptReportV2",
    "policy_id": "fixture.completion-proof.v1",
    "policy_runner_bundle_sha256": os.environ[
        "CODEX_COMPLETION_PROOF_POLICY_RUNNER_BUNDLE_SHA256"
    ],
    "exact_command": EXACT_COMMAND,
    "focused_validation_id": VALIDATION_ID,
    "nonce": os.environ["CODEX_COMPLETION_PROOF_NONCE"],
    "attempt_id": os.environ["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
    "parent_pid": int(os.environ["CODEX_COMPLETION_PROOF_PARENT_PID"]),
    "observed_runner_parent_pid": os.getppid(),
    "runner_process_identity": {
        "pid": os.getpid(),
        "parent_pid": os.getppid(),
        "started_at": runner_started_at,
        "ended_at": runner_ended_at,
        "args_hash": sha256(canonical_json([sys.executable, *sys.argv])),
        "executable_identity": runner_executable_identity,
        "entrypoint_identity": runner_entrypoint_identity,
    },
    "repository_root": os.environ["CODEX_COMPLETION_PROOF_REPOSITORY"],
    "host_identity": {
        "hostname": socket.gethostname() or "unknown",
        "system": platform.system() or "unknown",
        "release": platform.release() or "unknown",
        "machine": platform.machine() or "unknown",
    },
    "inventory_hash": inventory["inventory_hash"],
    "start_fingerprint": fingerprint,
    "end_fingerprint": fingerprint,
    "start_mutation_epoch": int(os.environ["CODEX_COMPLETION_PROOF_MUTATION_EPOCH"]),
    "end_mutation_epoch": int(os.environ["CODEX_COMPLETION_PROOF_MUTATION_EPOCH"]),
    "attempt_classification": classification,
    "validations": [validation],
    "child_processes": [{
        "validation_id": VALIDATION_ID,
        "execution_id": execution_id,
        "pid": child.pid,
        "executable": launch_target_identity["resolved_path"],
        "launch_target_identity": launch_target_identity,
        "args_hash": sha256(canonical_json(command)),
        "started_at": started_at,
        "ended_at": ended_at,
        "exit_code": child.returncode,
    }],
    "exceptions": [],
    "overrides": [],
    "workspace": {
        "observed_start_fingerprint": fingerprint,
        "observed_end_fingerprint": fingerprint,
    },
    "fatal_error": "",
}
report["attempt_report_hash"] = sha256(canonical_json(report))
report_path = Path(os.environ["CODEX_COMPLETION_PROOF_REPORT"])
report_path.parent.mkdir(parents=True, exist_ok=True)
report_path.write_text(json.dumps(report), encoding="utf-8")
raise SystemExit(child.returncode)
"#;

    TEMPLATE
        .replace(
            "__EXACT_COMMAND__",
            &serde_json::to_string(
                &focused_command_template.replace("{validation_id}", FIXTURE_VALIDATION_ID),
            )
            .expect("serialize focused command"),
        )
        .replace(
            "__FAILURE_PATH__",
            &serde_json::to_string(&failure_path.to_string_lossy())
                .expect("serialize focused failure path"),
        )
}

pub fn test_tmp_path() -> AbsolutePathBuf {
    test_absolute_path_with_windows("/tmp", Some(r"C:\Users\codex\AppData\Local\Temp"))
}

pub fn test_tmp_path_buf() -> PathBuf {
    test_tmp_path().into_path_buf()
}

pub fn workspace_write_excluding_tmp() -> PermissionProfile {
    PermissionProfile::workspace_write_with(
        &[],
        NetworkSandboxPolicy::Restricted,
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ true,
    )
}

pub fn requested_directory_write_permissions(path: &Path) -> RequestPermissionProfile {
    RequestPermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![PathUri::from_abs_path(
                &AbsolutePathBuf::try_from(path).expect("absolute path"),
            )]),
        )),
        ..RequestPermissionProfile::default()
    }
}

pub fn normalized_directory_write_permissions(
    path: &Path,
) -> anyhow::Result<RequestPermissionProfile> {
    Ok(RequestPermissionProfile {
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![]),
            Some(vec![PathUri::from_abs_path(&AbsolutePathBuf::try_from(
                path.canonicalize()?,
            )?)]),
        )),
        ..RequestPermissionProfile::default()
    })
}

/// Fetch a DotSlash resource and return the resolved executable/file path.
pub fn fetch_dotslash_file(
    dotslash_file: &std::path::Path,
    dotslash_cache: Option<&std::path::Path>,
) -> anyhow::Result<PathBuf> {
    let mut command = std::process::Command::new("dotslash");
    command.arg("--").arg("fetch").arg(dotslash_file);
    if let Some(dotslash_cache) = dotslash_cache {
        command.env("DOTSLASH_CACHE", dotslash_cache);
    }
    let output = command.output().with_context(|| {
        format!(
            "failed to run dotslash to fetch resource {}",
            dotslash_file.display()
        )
    })?;
    ensure!(
        output.status.success(),
        "dotslash fetch failed for {}: {}",
        dotslash_file.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let fetched_path = String::from_utf8(output.stdout)
        .context("dotslash fetch output was not utf8")?
        .trim()
        .to_string();
    ensure!(!fetched_path.is_empty(), "dotslash fetch output was empty");
    let fetched_path = PathBuf::from(fetched_path);
    ensure!(
        fetched_path.is_file(),
        "dotslash returned non-file path: {}",
        fetched_path.display()
    );
    Ok(fetched_path)
}

/// Returns a default `Config` whose on-disk state is confined to the provided
/// temporary directory. Using a per-test directory keeps tests hermetic and
/// avoids clobbering a developer’s real `~/.codex`.
pub async fn load_default_config_for_test(codex_home: &TempDir) -> Config {
    load_default_config_for_test_with_cloud_config_bundle(
        codex_home,
        CloudConfigBundleLoader::default(),
    )
    .await
}

/// Returns a default `Config` with test-provided cloud bundle requirements applied.
/// during config construction.
pub async fn load_default_config_for_test_with_cloud_config_bundle(
    codex_home: &TempDir,
    cloud_config_bundle: CloudConfigBundleLoader,
) -> Config {
    let mut config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(default_test_overrides(codex_home.path()))
        .cloud_config_bundle(cloud_config_bundle)
        .build()
        .await
        .expect("defaults for test should always succeed");
    // Do not let a developer-level CODEX_SQLITE_HOME override make otherwise
    // hermetic integration tests share one state database.
    config.sqlite_home = codex_home.path().join("sqlite");
    config
}

pub fn managed_network_requirements_loader() -> CloudConfigBundleLoader {
    CloudConfigBundleFixture::loader_with_enterprise_requirement(
        r#"
[experimental_network]
enabled = true
allow_local_binding = true
"#,
    )
}

fn default_test_overrides(codex_home: &Path) -> ConfigOverrides {
    ConfigOverrides {
        cwd: Some(codex_home.to_path_buf()),
        ..ConfigOverrides::default()
    }
}

pub async fn wait_for_event<F>(
    codex: &CodexThread,
    predicate: F,
) -> codex_protocol::protocol::EventMsg
where
    F: FnMut(&codex_protocol::protocol::EventMsg) -> bool,
{
    use tokio::time::Duration;
    wait_for_event_with_timeout(codex, predicate, Duration::from_secs(1)).await
}

/// Waits for a configured MCP server to finish startup and requires it to be ready.
pub async fn wait_for_mcp_server(codex: &CodexThread, server_name: &str) -> anyhow::Result<()> {
    use codex_protocol::protocol::EventMsg;

    // Wait for the startup summary regardless of outcome, then interpret the
    // requested server's ready, failed, or cancelled entry below.
    let summary = loop {
        let event = codex
            .next_event()
            .await
            .expect("stream ended unexpectedly while waiting for MCP startup");
        if let EventMsg::McpStartupComplete(summary) = event.msg {
            break summary;
        }
    };
    if let Some(failure) = summary
        .failed
        .iter()
        .find(|failure| failure.server == server_name)
    {
        let error = &failure.error;
        anyhow::bail!("MCP server {server_name} failed to start: {error}");
    }
    if summary.cancelled.iter().any(|server| server == server_name) {
        anyhow::bail!("MCP server {server_name} startup was cancelled");
    }
    assert!(
        summary.ready.iter().any(|server| server == server_name),
        "expected MCP server {server_name} to be ready; startup summary: {summary:?}"
    );
    Ok(())
}

pub async fn submit_thread_settings(
    codex: &CodexThread,
    thread_settings: codex_protocol::protocol::ThreadSettingsOverrides,
) -> anyhow::Result<()> {
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::Op;
    use tokio::time::Duration;
    use tokio::time::timeout;

    let submission_id = codex.submit(Op::ThreadSettings { thread_settings }).await?;
    loop {
        let ev = timeout(Duration::from_secs(10), codex.next_event())
            .await
            .expect("timeout waiting for thread settings update")
            .expect("stream ended unexpectedly");
        if ev.id == submission_id {
            match ev.msg {
                EventMsg::ThreadSettingsApplied(_) => return Ok(()),
                EventMsg::Error(err) => panic!("thread settings update failed: {}", err.message),
                other => panic!("unexpected thread settings update event: {other:?}"),
            }
        }
    }
}

pub async fn wait_for_event_match<T, F>(codex: &CodexThread, matcher: F) -> T
where
    F: Fn(&codex_protocol::protocol::EventMsg) -> Option<T>,
{
    wait_for_event_match_with_timeout(codex, matcher, INTEGRATION_EVENT_TIMEOUT).await
}

const INTEGRATION_EVENT_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(30);

pub async fn wait_for_event_match_with_timeout<T, F>(
    codex: &CodexThread,
    matcher: F,
    wait_time: tokio::time::Duration,
) -> T
where
    F: Fn(&codex_protocol::protocol::EventMsg) -> Option<T>,
{
    let ev = wait_for_event_with_timeout(codex, |ev| matcher(ev).is_some(), wait_time).await;
    matcher(&ev).expect("EventMsg should match matcher predicate")
}

pub async fn wait_for_event_with_timeout<F>(
    codex: &CodexThread,
    mut predicate: F,
    wait_time: tokio::time::Duration,
) -> codex_protocol::protocol::EventMsg
where
    F: FnMut(&codex_protocol::protocol::EventMsg) -> bool,
{
    loop {
        let ev = event_before_deadline(wait_time, codex.next_event())
            .await
            .expect("timeout waiting for event")
            .expect("stream ended unexpectedly");
        if predicate(&ev.msg) {
            return ev.msg;
        }
    }
}

async fn event_before_deadline<T>(
    wait_time: tokio::time::Duration,
    event: impl std::future::Future<Output = T>,
) -> Result<T, tokio::time::error::Elapsed> {
    tokio::time::timeout(wait_time, event).await
}

pub fn sandbox_env_var() -> &'static str {
    codex_core::spawn::CODEX_SANDBOX_ENV_VAR
}

pub fn sandbox_network_env_var() -> &'static str {
    codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
}

pub fn format_with_current_shell(command: &str) -> Vec<String> {
    codex_core::shell::default_user_shell()
        .derive_exec_args(command, /*use_login_shell*/ true)
        .expect("default Windows shell must be executable")
}

pub fn format_with_current_shell_display(command: &str) -> String {
    let args = format_with_current_shell(command);
    codex_app_server_protocol::command_display_string(&args)
}

pub fn format_with_current_shell_non_login(command: &str) -> Vec<String> {
    codex_core::shell::default_user_shell()
        .derive_exec_args(command, /*use_login_shell*/ false)
        .expect("default Windows shell must be executable")
}

pub fn format_with_current_shell_display_non_login(command: &str) -> String {
    let args = format_with_current_shell_non_login(command);
    codex_app_server_protocol::command_display_string(&args)
}

/// Resolves a helper binary the caller requires. Lookup failure is returned so
/// the caller propagates it; a required helper must never turn into a silent
/// pass. The owning test target declares the helper in
/// `codex-rs/.config/kd4-rust-tests.toml`, which builds it before the run.
pub fn required_helper_bin_with(
    name: &str,
    resolver: impl FnOnce(&str) -> Result<PathBuf, CargoBinError>,
) -> Result<String, CargoBinError> {
    resolver(name).map(|path| path.to_string_lossy().to_string())
}

pub fn required_helper_bin(name: &str) -> Result<String, CargoBinError> {
    required_helper_bin_with(name, codex_utils_cargo_bin::cargo_bin)
}

pub fn stdio_server_bin() -> Result<String, CargoBinError> {
    required_helper_bin("test_stdio_server")
}

pub mod fs_wait {
    use anyhow::Result;
    use anyhow::anyhow;
    use notify::RecursiveMode;
    use notify::Watcher;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;
    use std::time::Instant;
    use tokio::task;
    use walkdir::WalkDir;

    pub async fn wait_for_path_exists(
        path: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Result<PathBuf> {
        let path = path.into();
        task::spawn_blocking(move || wait_for_path_exists_blocking(path, timeout)).await?
    }

    pub async fn wait_for_matching_file(
        root: impl Into<PathBuf>,
        timeout: Duration,
        predicate: impl FnMut(&Path) -> bool + Send + 'static,
    ) -> Result<PathBuf> {
        let root = root.into();
        task::spawn_blocking(move || {
            let mut predicate = predicate;
            blocking_find_matching_file(root, timeout, &mut predicate)
        })
        .await?
    }

    fn wait_for_path_exists_blocking(path: PathBuf, timeout: Duration) -> Result<PathBuf> {
        if path.exists() {
            return Ok(path);
        }

        let watch_root = nearest_existing_ancestor(&path);
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        watcher.watch(&watch_root, RecursiveMode::Recursive)?;

        let deadline = Instant::now() + timeout;
        loop {
            if path.exists() {
                return Ok(path);
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            match rx.recv_timeout(remaining) {
                Ok(Ok(_event)) => {
                    if path.exists() {
                        return Ok(path);
                    }
                }
                Ok(Err(err)) => return Err(err.into()),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        if path.exists() {
            Ok(path)
        } else {
            Err(anyhow!("timed out waiting for {path:?}"))
        }
    }

    fn blocking_find_matching_file(
        root: PathBuf,
        timeout: Duration,
        predicate: &mut impl FnMut(&Path) -> bool,
    ) -> Result<PathBuf> {
        let root = wait_for_path_exists_blocking(root, timeout)?;

        if let Some(found) = scan_for_match(&root, predicate) {
            return Ok(found);
        }

        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        watcher.watch(&root, RecursiveMode::Recursive)?;

        let deadline = Instant::now() + timeout;

        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(Ok(_event)) => {
                    if let Some(found) = scan_for_match(&root, predicate) {
                        return Ok(found);
                    }
                }
                Ok(Err(err)) => return Err(err.into()),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        if let Some(found) = scan_for_match(&root, predicate) {
            Ok(found)
        } else {
            Err(anyhow!("timed out waiting for matching file in {root:?}"))
        }
    }

    fn scan_for_match(root: &Path, predicate: &mut impl FnMut(&Path) -> bool) -> Option<PathBuf> {
        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if !entry.file_type().is_file() {
                continue;
            }
            if predicate(path) {
                return Some(path.to_path_buf());
            }
        }
        None
    }

    fn nearest_existing_ancestor(path: &Path) -> PathBuf {
        let mut current = path;
        loop {
            if current.exists() {
                return current.to_path_buf();
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => return PathBuf::from("."),
            }
        }
    }
}

#[macro_export]
macro_rules! skip_if_no_network {
    () => {{
        if ::std::env::var($crate::sandbox_network_env_var()).is_ok() {
            println!(
                "Skipping test because it cannot execute when network is disabled in a Codex sandbox."
            );
            return;
        }
    }};
    ($return_value:expr $(,)?) => {{
        if ::std::env::var($crate::sandbox_network_env_var()).is_ok() {
            println!(
                "Skipping test because it cannot execute when network is disabled in a Codex sandbox."
            );
            return $return_value;
        }
    }};
}

// Exported so the public skip macros can expand in downstream test crates.
#[macro_export]
#[doc(hidden)]
macro_rules! skip_if_test_condition {
    ($condition:expr, $environment:expr, $reason:expr $(,)?) => {{
        if $condition {
            eprintln!("Skipping test in {}: {}", $environment, $reason);
            return;
        }
    }};
    ($return_value:expr, $condition:expr, $environment:expr, $reason:expr $(,)?) => {{
        if $condition {
            eprintln!("Skipping test in {}: {}", $environment, $reason);
            return $return_value;
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;
    use std::time::Instant;

    #[test]
    fn host_path_fixture_uses_host_convention() {
        let path = test_path_buf_with_windows("/tmp/kd4-test", Some(r"C:\tmp\kd4-test"));

        assert_eq!(path, PathBuf::from(r"C:\tmp\kd4-test"));
    }

    #[tokio::test]
    async fn default_config_keeps_multi_agent_v2_enabled() {
        let codex_home = tempfile::tempdir().expect("create test Codex home");
        let config = load_default_config_for_test(&codex_home).await;

        assert!(
            config
                .features
                .enabled(codex_features::Feature::MultiAgentV2)
        );
    }

    #[tokio::test]
    async fn event_waiter_honors_requested_deadline() {
        let wait_time = tokio::time::Duration::from_millis(10);
        let started = Instant::now();

        let result = event_before_deadline(wait_time, pending::<()>()).await;

        assert!(result.is_err());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "requested short deadline was widened: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn integration_event_timeout_allows_loaded_shards_to_make_progress() {
        assert_eq!(
            INTEGRATION_EVENT_TIMEOUT,
            tokio::time::Duration::from_secs(30)
        );
    }
}
