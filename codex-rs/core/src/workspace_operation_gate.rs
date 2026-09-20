use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;

use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::OwnedMutexGuard;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum WorkspaceIdentity {
    Local(PathBuf),
    Environment(String),
}

static WORKSPACE_GATES: OnceLock<Mutex<HashMap<WorkspaceIdentity, Weak<AsyncMutex<()>>>>> =
    OnceLock::new();

pub(crate) async fn acquire_patch_operation(
    environment: &codex_exec_server::Environment,
    cwd: &codex_utils_path_uri::PathUri,
) -> OwnedMutexGuard<()> {
    if !environment.is_remote()
        && let Ok(native_cwd) = cwd.to_abs_path()
    {
        let root = codex_git_utils::get_git_repo_root(&native_cwd)
            .unwrap_or_else(|| native_cwd.to_path_buf());
        return acquire_workspace_operation(&root).await;
    }
    // Remote paths must never be resolved on the host. Serialize the concrete
    // environment conservatively, including calls from different working dirs.
    acquire_operation(WorkspaceIdentity::Environment(
        environment.approval_scope_id().to_string(),
    ))
    .await
}

pub(crate) async fn acquire_workspace_operation(root: &Path) -> OwnedMutexGuard<()> {
    let identity = tokio::fs::canonicalize(root)
        .await
        .map(|path| dunce::simplified(&path).to_path_buf())
        .unwrap_or_else(|_| root.to_path_buf());
    acquire_operation(WorkspaceIdentity::Local(identity)).await
}

async fn acquire_operation(identity: WorkspaceIdentity) -> OwnedMutexGuard<()> {
    let gate = {
        let mut gates = WORKSPACE_GATES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(&identity).and_then(Weak::upgrade) {
            gate
        } else {
            let gate = Arc::new(AsyncMutex::new(()));
            gates.insert(identity, Arc::downgrade(&gate));
            gate
        }
    };
    gate.lock_owned().await
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
