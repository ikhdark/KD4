use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::time::Instant;

use codex_protocol::ThreadId;

use crate::scoped_workspace_gate::NativeAdditions;
use crate::scoped_workspace_gate::ScopedWorkspaceGate;
use crate::scoped_workspace_gate::WorkspaceLease;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum WorkspaceIdentity {
    Local(PathBuf),
    Environment(String),
    Validation(Box<WorkspaceIdentity>),
}

/// The thread asking for validation admission.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ValidationOwner {
    thread_id: ThreadId,
}

impl ValidationOwner {
    pub(crate) fn for_command(thread_id: ThreadId, _command: &[String]) -> Self {
        // Command syntax cannot prove resource isolation: Python tests can
        // compile native code or overwrite the same workspace fixtures.
        Self { thread_id }
    }
}

static WORKSPACE_GATES: OnceLock<Mutex<HashMap<WorkspaceIdentity, Weak<ValidationGate>>>> =
    OnceLock::new();
static PATCH_GATES: OnceLock<Mutex<HashMap<WorkspaceIdentity, Weak<ScopedWorkspaceGate>>>> =
    OnceLock::new();

#[derive(Debug, Default)]
struct ValidationGate {
    holder: Mutex<Option<ValidationHolder>>,
}

#[derive(Debug)]
struct ValidationHolder {
    command: String,
    admitted_at: Instant,
    owner: ThreadId,
}

#[derive(Debug)]
pub(crate) struct ValidationPermit {
    // Keep the weak-map entry alive for the entire admitted operation.
    gate: Arc<ValidationGate>,
}

impl Drop for ValidationPermit {
    fn drop(&mut self) {
        // Holder state IS the admission lock: clear it under the same mutex
        // used for admission, before unlocking. No departing owner can erase
        // a successor, and no async guard or second lock order is needed.
        self.gate
            .holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

pub(crate) async fn acquire_patch_operation_with_timeout(
    environment: &codex_exec_server::Environment,
    cwd: &codex_utils_path_uri::PathUri,
) -> Result<WorkspaceLease, &'static str> {
    acquire_patch_additions_with_timeout(environment, cwd, None).await
}

pub(crate) async fn acquire_patch_additions_with_timeout(
    environment: &codex_exec_server::Environment,
    cwd: &codex_utils_path_uri::PathUri,
    additions: Option<&[PathBuf]>,
) -> Result<WorkspaceLease, &'static str> {
    // Only other patches retain this gate. Validation uses an independent lane
    // and its existing freshness check rejects results invalidated by a patch.
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        async {
            if !environment.is_remote()
                && let Ok(native_cwd) = cwd.to_abs_path()
                && let Some(paths) = additions
            {
                let root = codex_git_utils::get_git_repo_root(&native_cwd)
                    .unwrap_or_else(|| native_cwd.to_path_buf());
                if let Some(footprint) = NativeAdditions::resolve(&root, &native_cwd, paths).await {
                    return footprint.acquire(patch_gate(local_identity(&root).await)).await;
                }
            }
            acquire_patch_operation(environment, cwd).await
        },
    )
    .await
    .map_err(|_| "apply_patch: workspace is busy with another patch (waited 10 seconds). No files were changed by this attempt. Wait for the active operation to finish before retrying; do not restart it or bypass the workspace lock.")
}

pub(crate) async fn acquire_patch_operation(
    environment: &codex_exec_server::Environment,
    cwd: &codex_utils_path_uri::PathUri,
) -> WorkspaceLease {
    if !environment.is_remote()
        && let Ok(native_cwd) = cwd.to_abs_path()
    {
        let root = codex_git_utils::get_git_repo_root(&native_cwd)
            .unwrap_or_else(|| native_cwd.to_path_buf());
        return acquire_workspace_operation(&root).await;
    }
    // Remote paths must never be resolved on the host. Serialize the concrete
    // environment conservatively, including calls from different working dirs.
    patch_gate(WorkspaceIdentity::Environment(
        environment.approval_scope_id().to_string(),
    ))
    .write_owned()
    .await
}

pub(crate) async fn acquire_workspace_operation(root: &Path) -> WorkspaceLease {
    patch_gate(local_identity(root).await).write_owned().await
}

fn patch_gate(identity: WorkspaceIdentity) -> Arc<ScopedWorkspaceGate> {
    let mut gates = PATCH_GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(gate) = gates.get(&identity).and_then(Weak::upgrade) {
        return gate;
    }
    gates.retain(|_, gate| gate.strong_count() > 0);
    let gate = Arc::new(ScopedWorkspaceGate::default());
    gates.insert(identity, Arc::downgrade(&gate));
    gate
}

/// Try admission without queueing before a process handle exists. Contention
/// leaves validation pending; it must not consume the tool's launch deadline.
pub(crate) async fn acquire_validation_operation(
    environment: &codex_exec_server::Environment,
    cwd: &codex_utils_path_uri::PathUri,
    command: &str,
    owner: ValidationOwner,
) -> Result<ValidationPermit, String> {
    let identity = if !environment.is_remote()
        && let Ok(native_cwd) = cwd.to_abs_path()
    {
        let root = codex_git_utils::get_git_repo_root(&native_cwd)
            .unwrap_or_else(|| native_cwd.to_path_buf());
        local_identity(&root).await
    } else {
        WorkspaceIdentity::Environment(environment.approval_scope_id().to_string())
    };
    try_acquire_operation(identity, command, owner)
}

pub(crate) async fn acquire_workspace_validation(
    root: &Path,
    command: &str,
    owner: ValidationOwner,
) -> Result<ValidationPermit, String> {
    try_acquire_operation(local_identity(root).await, command, owner)
}

async fn local_identity(root: &Path) -> WorkspaceIdentity {
    let identity = tokio::fs::canonicalize(root)
        .await
        .map(|path| dunce::simplified(&path).to_path_buf())
        .unwrap_or_else(|_| root.to_path_buf());
    WorkspaceIdentity::Local(identity)
}

fn try_acquire_operation(
    workspace: WorkspaceIdentity,
    command: &str,
    owner: ValidationOwner,
) -> Result<ValidationPermit, String> {
    let identity = WorkspaceIdentity::Validation(Box::new(workspace));
    let gate = {
        let mut gates = WORKSPACE_GATES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(&identity).and_then(Weak::upgrade) {
            gate
        } else {
            let gate = Arc::new(ValidationGate::default());
            gates.insert(identity, Arc::downgrade(&gate));
            gate
        }
    };
    let mut holder = gate
        .holder
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(holder) = holder.as_ref() {
        let next_step = if holder.owner == owner.thread_id {
            "Resume the existing operation rather than relaunching or switching lanes."
        } else {
            "The holder belongs to another session or agent, so it cannot be resumed from here; retry after it finishes."
        };
        return Err(format!(
            "Validation busy: holder command {:?} (held for {:.3} seconds). This validation did not run. No process was started or queued; required validation remains pending. {next_step}",
            holder.command,
            holder.admitted_at.elapsed().as_secs_f64(),
        ));
    }
    *holder = Some(ValidationHolder {
        command: command.to_owned(),
        admitted_at: Instant::now(),
        owner: owner.thread_id,
    });
    drop(holder);
    Ok(ValidationPermit { gate })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cargo_owner(thread_id: ThreadId) -> ValidationOwner {
        ValidationOwner::for_command(thread_id, &["cargo".to_string(), "test".to_string()])
    }

    #[tokio::test]
    async fn remote_patch_gates_follow_environment_identity_not_host_paths() {
        use codex_exec_server::Environment;
        use codex_utils_path_uri::PathUri;
        use std::time::Duration;

        let first = Environment::create_for_tests(Some("ws://127.0.0.1:1".into())).unwrap();
        let second = Environment::create_for_tests(Some("ws://127.0.0.1:2".into())).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let native = PathUri::from_host_native_path(temp.path()).unwrap();
        let foreign = if cfg!(windows) {
            PathUri::parse("file:///remote/workspace").unwrap()
        } else {
            PathUri::parse("file:///C:/remote/workspace").unwrap()
        };
        let held = acquire_patch_operation(&first, &native).await;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                acquire_patch_operation(&first, &foreign)
            )
            .await
            .is_err()
        );
        let _independent = tokio::time::timeout(
            Duration::from_secs(1),
            acquire_patch_operation(&second, &native),
        )
        .await
        .unwrap();
        let _host = tokio::time::timeout(
            Duration::from_secs(1),
            acquire_workspace_operation(temp.path()),
        )
        .await
        .unwrap();
        drop(held);
        tokio::time::timeout(
            Duration::from_secs(1),
            acquire_patch_operation(&first, &foreign),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn same_workspace_operations_are_serialized() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let first = acquire_workspace_operation(temp.path()).await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                acquire_workspace_operation(&temp.path().join(".")),
            )
            .await
            .is_err()
        );
        drop(first);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            acquire_workspace_operation(temp.path()),
        )
        .await
        .expect("released workspace should become available");
    }

    #[tokio::test]
    async fn different_workspaces_remain_concurrent() {
        let first = tempfile::tempdir().expect("first workspace");
        let second = tempfile::tempdir().expect("second workspace");
        let _first = acquire_workspace_operation(first.path()).await;
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            acquire_workspace_operation(second.path()),
        )
        .await
        .expect("different workspace should not wait");
    }

    #[tokio::test]
    async fn validation_serializes_validations_but_not_patches() {
        let temp = tempfile::tempdir().unwrap();
        let environment = codex_exec_server::Environment::default_for_tests();
        let cwd = codex_utils_path_uri::PathUri::from_host_native_path(temp.path()).unwrap();
        let owner = cargo_owner(ThreadId::new());
        let validation =
            acquire_validation_operation(&environment, &cwd, "cargo test -p owner", owner)
                .await
                .unwrap();
        // A known admission age proves the diagnostic uses this holder's clock.
        validation
            .gate
            .holder
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .admitted_at = Instant::now() - std::time::Duration::from_secs(2);
        let busy = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            acquire_workspace_validation(temp.path(), "cargo check -p contender", owner),
        )
        .await
        .expect("contended validation must return, not queue")
        .unwrap_err();
        assert!(busy.contains("No process was started or queued"));
        assert!(busy.contains("cargo test -p owner"));
        assert!(busy.contains("This validation did not run"));
        let age = busy
            .split("held for ")
            .nth(1)
            .unwrap()
            .split(" seconds")
            .next()
            .unwrap()
            .parse::<f64>()
            .unwrap();
        assert!(age >= 2.0, "holder age must start at admission: {busy}");
        assert!(
            acquire_validation_operation(&environment, &cwd, "cargo check", owner)
                .await
                .is_err(),
            "a rejected contender must not release the owner's permit"
        );
        let patch = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            acquire_patch_operation(&environment, &cwd),
        )
        .await
        .expect("a running validation must not block a patch");
        drop(validation);
        let next_validation = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            acquire_workspace_validation(temp.path(), "cargo check -p successor", owner),
        )
        .await
        .expect("a running patch must not block validation")
        .unwrap();
        let busy = acquire_validation_operation(&environment, &cwd, "cargo test", owner)
            .await
            .unwrap_err();
        assert!(busy.contains("cargo check -p successor"));
        assert!(!busy.contains("cargo test -p owner"));
        drop(next_validation);
        assert!(
            acquire_workspace_validation(temp.path(), "cargo test -p final", owner)
                .await
                .is_ok()
        );
        drop(patch);
    }

    #[tokio::test]
    async fn validation_busy_is_scoped_to_workspace_and_remote_environment() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let thread = cargo_owner(ThreadId::new());
        let _owner = acquire_workspace_validation(first.path(), "cargo test -p owner", thread)
            .await
            .unwrap();
        assert!(
            acquire_workspace_validation(&first.path().join("."), "cargo check", thread)
                .await
                .is_err()
        );
        let _independent = acquire_workspace_validation(second.path(), "cargo check", thread)
            .await
            .unwrap();
        let remote =
            codex_exec_server::Environment::create_for_tests(Some("ws://127.0.0.1:1".into()))
                .unwrap();
        let other =
            codex_exec_server::Environment::create_for_tests(Some("ws://127.0.0.1:2".into()))
                .unwrap();
        let cwd = codex_utils_path_uri::PathUri::from_host_native_path(first.path()).unwrap();
        let other_cwd =
            codex_utils_path_uri::PathUri::from_host_native_path(second.path()).unwrap();
        let owner = acquire_validation_operation(&remote, &cwd, "cargo test -p remote", thread)
            .await
            .unwrap();
        let busy = acquire_validation_operation(&remote, &other_cwd, "cargo check", thread)
            .await
            .unwrap_err();
        assert!(busy.contains("cargo test -p remote"));
        let _other = acquire_validation_operation(&other, &cwd, "cargo check", thread)
            .await
            .unwrap();
        drop(owner);
        assert!(
            acquire_validation_operation(&remote, &other_cwd, "cargo check", thread)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn python_validation_cannot_overwrite_another_threads_live_fixture() {
        use std::process::Stdio;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("test_shared_validation.py"),
            r#"import os, pathlib, time, unittest

class SharedValidation(unittest.TestCase):
    def test_shared_output(self):
        role = os.environ['VALIDATION_FIXTURE_ROLE']
        pathlib.Path('shared-output').write_text(role)
        if role == 'owner':
            pathlib.Path('ready').touch()
            deadline = time.monotonic() + 15
            while not pathlib.Path('release').exists():
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.01)
            self.assertEqual(pathlib.Path('shared-output').read_text(), 'owner')
"#,
        )
        .unwrap();
        let python = which::which("python")
            .or_else(|_| which::which("python3"))
            .expect("Python fixture interpreter");
        let argv = [
            python.to_string_lossy().into_owned(),
            "-B".into(),
            "-m".into(),
            "unittest".into(),
            "test_shared_validation".into(),
        ];
        let command = argv.join(" ");
        let environment = codex_exec_server::Environment::default_for_tests();
        let cwd = codex_utils_path_uri::PathUri::from_host_native_path(temp.path()).unwrap();
        let held = acquire_validation_operation(
            &environment,
            &cwd,
            &command,
            ValidationOwner::for_command(ThreadId::new(), &argv),
        )
        .await
        .unwrap();
        let child = |role: &str| {
            let mut child = tokio::process::Command::new(&python);
            child
                .args(&argv[1..])
                .current_dir(temp.path())
                .env("VALIDATION_FIXTURE_ROLE", role)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            child
        };
        let owner = child("owner").spawn().unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !temp.path().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("real unittest must hold the shared fixture before contention");
        let contender = tokio::time::timeout(
            Duration::from_secs(1),
            acquire_workspace_validation(
                temp.path(),
                &command,
                ValidationOwner::for_command(ThreadId::new(), &argv),
            ),
        )
        .await
        .expect("busy admission must not queue behind the live fixture");
        // If admission regresses, execute the admitted test to expose the real
        // shared-file collision, not just a difference in classifier output.
        if contender.is_ok() {
            let output = tokio::time::timeout(Duration::from_secs(5), child("contender").output())
                .await
                .unwrap()
                .unwrap();
            assert!(output.status.success(), "{:?}", output.stderr);
        }
        std::fs::write(temp.path().join("release"), b"release").unwrap();
        let output = tokio::time::timeout(Duration::from_secs(5), owner.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "live validation fixture was overwritten: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(contender.is_err(), "competing validation must not be admitted");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("shared-output")).unwrap(),
            "owner"
        );
        drop(held);
        assert!(
            acquire_workspace_validation(
                temp.path(),
                &command,
                ValidationOwner::for_command(ThreadId::new(), &argv),
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn python_validation_contends_with_shared_workspace_validation() {
        let temp = tempfile::tempdir().unwrap();
        let python = ["python", "-m", "pytest", "tests"].map(str::to_string);
        let (first, second) = (ThreadId::new(), ThreadId::new());
        let first_python = ValidationOwner::for_command(first, &python);
        let held = acquire_workspace_validation(temp.path(), "python -m pytest tests", first_python)
            .await
            .unwrap();
        // Test modules and plugins may compile or write shared fixtures. The
        // interpreter name is not evidence that their resources are isolated.
        let foreign = acquire_workspace_validation(
            temp.path(),
            "python -m pytest tests",
            ValidationOwner::for_command(second, &python),
        )
        .await
        .unwrap_err();
        assert!(foreign.contains("belongs to another session or agent"), "{foreign}");
        let duplicate = acquire_workspace_validation(temp.path(), "python -m pytest", first_python)
            .await
            .unwrap_err();
        assert!(duplicate.contains("Resume the existing operation"), "{duplicate}");
        let cargo = acquire_workspace_validation(temp.path(), "cargo test", cargo_owner(first))
            .await
            .unwrap_err();
        assert!(cargo.contains("Resume the existing operation"), "{cargo}");
        drop(held);
        let cargo = acquire_workspace_validation(temp.path(), "cargo test", cargo_owner(first))
            .await
            .unwrap();
        let foreign = acquire_workspace_validation(temp.path(), "cargo check", cargo_owner(second))
            .await
            .unwrap_err();
        assert!(foreign.contains("\"cargo test\""), "{foreign}");
        assert!(foreign.contains("This validation did not run"), "{foreign}");
        assert!(foreign.contains("belongs to another session or agent"), "{foreign}");
        assert!(!foreign.contains("Resume the existing operation"), "{foreign}");
        drop(cargo);
        assert!(
            acquire_workspace_validation(temp.path(), "cargo check", cargo_owner(second))
                .await
                .is_ok()
        );
    }
}
