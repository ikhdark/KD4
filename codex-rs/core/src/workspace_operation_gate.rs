use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;

use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::OnceCell;
use tokio::sync::OwnedMutexGuard;

static WORKSPACE_GATES: OnceLock<Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>> = OnceLock::new();

pub(crate) async fn acquire_workspace_operation(root: &Path) -> OwnedMutexGuard<()> {
    let identity = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
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

/// A cloneable claim on one workspace operation.
///
/// The first runtime attempt acquires the underlying gate. Every clone then
/// keeps that same permit alive until command-end mutation bookkeeping has
/// completed.
#[derive(Clone)]
pub(crate) struct WorkspaceOperationLease {
    root: PathBuf,
    permit: Arc<OnceCell<OwnedMutexGuard<()>>>,
}

impl WorkspaceOperationLease {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            permit: Arc::new(OnceCell::new()),
        }
    }

    pub(crate) async fn acquire(&self) {
        let root = self.root.clone();
        self.permit
            .get_or_init(|| async move { acquire_workspace_operation(&root).await })
            .await;
    }
}

impl fmt::Debug for WorkspaceOperationLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceOperationLease")
            .field("root", &self.root)
            .field("acquired", &self.permit.get().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn same_workspace_operations_are_serialized() {
        let temp = tempfile::tempdir().expect("temp workspace");
        let first = acquire_workspace_operation(temp.path()).await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                acquire_workspace_operation(temp.path()),
            )
            .await
            .is_err()
        );
        drop(first);
        acquire_workspace_operation(temp.path()).await;
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
