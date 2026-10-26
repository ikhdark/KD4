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
