use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::time::Duration;

use codex_protocol::protocol::ToolLifecycleTimerWait;
use codex_protocol::protocol::ToolLifecycleWakeReason;

use crate::scoped_workspace_gate::NativeAdditions;
use crate::scoped_workspace_gate::ScopedWorkspaceGate;
use crate::scoped_workspace_gate::WorkspaceLease;


#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum WorkspaceIdentity {
    Local(PathBuf),
    Environment(String),
}

static PATCH_GATES: OnceLock<Mutex<HashMap<WorkspaceIdentity, Weak<ScopedWorkspaceGate>>>> =
    OnceLock::new();

/// Longest a patch waits behind another patch in the same workspace.
const PATCH_GATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Holders a patch busy refusal names; its count still covers all of them.
const PATCH_GATE_HOLDER_LIMIT: usize = 4;

/// Uncontended admission (identity resolution and an immediate grant) stays
/// well below this, so shorter waits are not persisted as contention.
const GATE_WAIT_RECORD_THRESHOLD: Duration = Duration::from_millis(100);

/// Persists a contended admission wait with the active tool dispatch, so turn
/// profiles separate waiting behind another call, session or process from the
/// tool's own work. A wait dropped before it resolves was cancelled.
struct GateWaitRecord {
    wait_kind: &'static str,
    requested_timeout: Option<Duration>,
    started: tokio::time::Instant,
    wake_reason: ToolLifecycleWakeReason,
}

impl GateWaitRecord {
    fn begin(wait_kind: &'static str, requested_timeout: Option<Duration>) -> Self {
        Self {
            wait_kind,
            requested_timeout,
            started: tokio::time::Instant::now(),
            wake_reason: ToolLifecycleWakeReason::Cancelled,
        }
    }

    fn finish(mut self, wake_reason: ToolLifecycleWakeReason) {
        self.wake_reason = wake_reason;
    }
}

impl Drop for GateWaitRecord {
    fn drop(&mut self) {
        let waited = self.started.elapsed();
        if waited < GATE_WAIT_RECORD_THRESHOLD {
            return;
        }
        let Some(timing) = crate::tools::tool_dispatch_trace::active_tool_dispatch_timing() else {
            return;
        };
        timing.record_timer_wait(ToolLifecycleTimerWait {
            wait_kind: self.wait_kind.to_string(),
            requested_timeout_ms: self.requested_timeout.and_then(duration_ms),
            effective_timeout_ms: duration_ms(waited),
            wake_reason: self.wake_reason,
            ..Default::default()
        });
    }
}

fn duration_ms(duration: Duration) -> Option<u64> {
    u64::try_from(duration.as_millis()).ok()
}

pub(crate) async fn acquire_patch_operation_with_timeout(
    environment: &codex_exec_server::Environment,
    cwd: &codex_utils_path_uri::PathUri,
) -> Result<WorkspaceLease, String> {
    acquire_patch_additions_with_timeout(environment, cwd, None).await
}

pub(crate) async fn acquire_patch_additions_with_timeout(
    environment: &codex_exec_server::Environment,
    cwd: &codex_utils_path_uri::PathUri,
    additions: Option<&[PathBuf]>,
) -> Result<WorkspaceLease, String> {
    let record = GateWaitRecord::begin("patch_gate", Some(PATCH_GATE_TIMEOUT));
    let acquired = tokio::time::timeout(PATCH_GATE_TIMEOUT, async {
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
    })
    .await;
    match acquired {
        Ok(lease) => {
            record.finish(ToolLifecycleWakeReason::Completed);
            Ok(lease)
        }
        Err(_) => {
            record.finish(ToolLifecycleWakeReason::Timeout);
            // The expired wait already withdrew its own claim, so this names
            // only operations that still hold the workspace.
            let gate = patch_gate(patch_workspace_identity(environment, cwd).await);
            Err(patch_busy_message(&gate, PATCH_GATE_TIMEOUT))
        }
    }
}

/// Names the attributed holders: a peer session's call cannot be resumed
/// from here, but its tool, age and access mode explain the refusal.
fn patch_busy_message(gate: &ScopedWorkspaceGate, waited: Duration) -> String {
    let (count, holders) = gate.holders(PATCH_GATE_HOLDER_LIMIT);
    let mut message = format!(
        "apply_patch: workspace is busy with another patch (waited {} seconds). No files were changed by this attempt.",
        waited.as_secs(),
    );
    if holders.is_empty() {
        message.push_str(" No current holder is attributed to a tool call.");
    } else {
        for holder in &holders {
            let mode = if holder.shared { "shared" } else { "exclusive" };
            message.push_str(&format!(
                " Held by `{}` (call {}), {mode}, for {:.1} seconds.",
                holder.call.tool_name,
                holder.call.call_id,
                holder.admitted_at.elapsed().as_secs_f64(),
            ));
        }
        if count > holders.len() {
            message.push_str(&format!(" {} more holders omitted.", count - holders.len()));
        }
        message.push_str(
            " A holder call that is not one of yours belongs to another session or agent in this checkout.",
        );
    }
    message.push_str(
        " Wait for the active operation to finish before retrying; do not restart it or bypass the workspace lock.",
    );
    message
}

/// The gate identity `acquire_patch_operation` serializes on.
async fn patch_workspace_identity(
    environment: &codex_exec_server::Environment,
    cwd: &codex_utils_path_uri::PathUri,
) -> WorkspaceIdentity {
    if !environment.is_remote()
        && let Ok(native_cwd) = cwd.to_abs_path()
    {
        let root = codex_git_utils::get_git_repo_root(&native_cwd)
            .unwrap_or_else(|| native_cwd.to_path_buf());
        return local_identity(&root).await;
    }
    WorkspaceIdentity::Environment(environment.coordination_resource_id())
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
        environment.coordination_resource_id(),
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

async fn local_identity(root: &Path) -> WorkspaceIdentity {
    let identity = tokio::fs::canonicalize(root)
        .await
        .map(|path| dunce::simplified(&path).to_path_buf())
        .unwrap_or_else(|_| root.to_path_buf());
    WorkspaceIdentity::Local(identity)
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

    fn recorded_waits(
        timing: &crate::tools::tool_dispatch_trace::ToolDispatchTiming,
    ) -> Vec<(String, ToolLifecycleWakeReason, Option<u64>, Option<u64>)> {
        timing
            .snapshot(tokio::time::Instant::now())
            .timer_waits
            .into_iter()
            .map(|wait| {
                (
                    wait.wait_kind,
                    wait.wake_reason,
                    wait.requested_timeout_ms,
                    wait.effective_timeout_ms,
                )
            })
            .collect()
    }

    /// Audit 151: a patch that times out behind a peer session's patch names
    /// that holder, and the wait joins the waiting dispatch's timing profile.
    #[tokio::test(start_paused = true)]
    async fn audit151_patch_gate_timeout_names_the_holder_and_joins_the_dispatch_profile() {
        use crate::scoped_workspace_gate::WORKSPACE_GATE_CALL;
        use crate::scoped_workspace_gate::WorkspaceGateCall;
        use crate::tools::tool_dispatch_trace::ToolDispatchTiming;
        use crate::tools::tool_dispatch_trace::scope_tool_dispatch_timing;

        let temp = tempfile::tempdir().unwrap();
        // Pin the repository root to this workspace rather than an ancestor.
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let environment = codex_exec_server::Environment::default_for_tests();
        let cwd = codex_utils_path_uri::PathUri::from_host_native_path(temp.path()).unwrap();
        let timing = Arc::new(ToolDispatchTiming::new(tokio::time::Instant::now(), false));
        // Negative control: the peer's immediate admission is not a wait.
        let peer = scope_tool_dispatch_timing(
            Arc::clone(&timing),
            WORKSPACE_GATE_CALL.scope(
                WorkspaceGateCall {
                    tool_name: "apply_patch".to_string(),
                    call_id: "call-peer-session".to_string(),
                },
                acquire_patch_operation_with_timeout(&environment, &cwd),
            ),
        )
        .await
        .unwrap();
        assert_eq!(recorded_waits(&timing), Vec::new());

        let started = tokio::time::Instant::now();
        let busy = scope_tool_dispatch_timing(
            Arc::clone(&timing),
            acquire_patch_operation_with_timeout(&environment, &cwd),
        )
        .await
        .unwrap_err();
        assert_eq!(started.elapsed(), PATCH_GATE_TIMEOUT);
        assert!(
            busy.contains("No files were changed by this attempt"),
            "{busy}"
        );
        assert!(
            busy.contains(
                "Held by `apply_patch` (call call-peer-session), exclusive, for 10.0 seconds"
            ),
            "{busy}"
        );
        assert!(busy.contains("another session or agent"), "{busy}");
        assert_eq!(
            recorded_waits(&timing),
            vec![(
                "patch_gate".to_string(),
                ToolLifecycleWakeReason::Timeout,
                Some(10_000),
                Some(10_000),
            )]
        );
        drop(peer);
    }

    #[tokio::test]
    async fn audit151_patch_busy_refusal_does_not_invent_an_unattributed_holder() {
        let gate = Arc::new(ScopedWorkspaceGate::default());
        let unattributed = Arc::clone(&gate).write_owned().await;
        let busy = patch_busy_message(&gate, PATCH_GATE_TIMEOUT);
        assert!(busy.contains("(waited 10 seconds)"), "{busy}");
        assert!(busy.contains("No current holder is attributed"), "{busy}");
        assert!(!busy.contains("Held by"), "{busy}");
        assert!(
            busy.contains("do not restart it or bypass the workspace lock"),
            "{busy}"
        );
        drop(unattributed);
    }

}
