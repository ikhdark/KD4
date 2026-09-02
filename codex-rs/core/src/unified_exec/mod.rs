//! Unified Exec: interactive process execution orchestrated with approvals + sandboxing.
//!
//! Responsibilities
//! - Manages interactive processes (create, reuse, buffer output with caps).
//! - Uses the shared ToolOrchestrator to handle approval, sandbox selection, and
//!   retry semantics in a single, descriptive flow.
//! - Spawns the PTY from a sandbox-transformed `ExecRequest`; on sandbox denial,
//!   retries without sandbox when policy allows (no re‑prompt thanks to caching).
//! - Uses the shared `is_likely_sandbox_denied` heuristic to keep denial messages
//!   consistent with other exec paths.
//!
//! Flow at a glance (open process)
//! 1) Build a small request `{ command, cwd }`.
//! 2) Orchestrator: approval (bypass/cache/prompt) → select sandbox → run.
//! 3) Runtime: transform `SandboxTransformRequest` -> `ExecRequest` -> spawn PTY.
//! 4) If denial, orchestrator retries with `SandboxType::None`.
//! 5) Process handle is returned with streaming output + metadata.
//!
//! This keeps policy logic and user interaction centralized while the PTY/process
//! concerns remain isolated here. The implementation is split between:
//! - `process.rs`: PTY process lifecycle + output buffering.
//! - `process_state.rs`: shared exit/failure state for local and remote processes.
//! - `process_manager.rs`: orchestration (approvals, sandboxing, reuse) and request handling.

use std::collections::HashMap;
use std::collections::HashSet;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;

use codex_network_proxy::NetworkProxy;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::request_permissions::UriAdditionalPermissionProfile;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_path_uri::PathUri;
use rand::Rng;
use rand::rng;
use tokio::sync::Mutex;
use tokio::sync::oneshot;

use crate::sandboxing::SandboxPermissions;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolCallSource;
use crate::tools::known_delta_store::PreparedKnownDelta;
use crate::tools::network_approval::DeferredNetworkApproval;

mod async_watcher;
mod errors;
mod head_tail_buffer;
mod process;
mod process_manager;
mod process_state;

pub(crate) fn set_deterministic_process_ids_for_tests(enabled: bool) {
    process_manager::set_deterministic_process_ids_for_tests(enabled);
}

pub(crate) use errors::UnifiedExecError;
pub(crate) use process::NoopSpawnLifecycle;
pub(crate) use process::SpawnLifecycle;
pub(crate) use process::SpawnLifecycleHandle;
pub(crate) use process::UnifiedExecProcess;

pub(crate) const MIN_YIELD_TIME_MS: u64 = 250;
pub(crate) const WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS: u64 = 2_000;
// Minimum yield time for an empty `write_stdin`.
pub(crate) const MIN_EMPTY_YIELD_TIME_MS: u64 = 5_000;
pub(crate) const MAX_YIELD_TIME_MS: u64 = 30_000;
pub(crate) const DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS: u64 = 60_000;

pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_BYTES: usize = 1024 * 1024; // 1 MiB
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_TOKENS: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES / 4;
pub(crate) const MAX_UNIFIED_EXEC_PROCESSES: usize = 64;

pub(crate) struct UnifiedExecContext {
    pub session: Arc<Session>,
    pub turn: Arc<TurnContext>,
    pub call_id: String,
    pub tracker: Option<SharedTurnDiffTracker>,
    pub source: ToolCallSource,
}

impl UnifiedExecContext {
    #[cfg(test)]
    pub fn new(session: Arc<Session>, turn: Arc<TurnContext>, call_id: String) -> Self {
        Self {
            session,
            turn,
            call_id,
            tracker: None,
            source: ToolCallSource::Direct,
        }
    }

    pub fn with_tracker(
        session: Arc<Session>,
        turn: Arc<TurnContext>,
        call_id: String,
        tracker: SharedTurnDiffTracker,
        source: ToolCallSource,
    ) -> Self {
        Self {
            session,
            turn,
            call_id,
            tracker: Some(tracker),
            source,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExecCommandRequest {
    pub command: Vec<String>,
    pub command_for_safety: Vec<String>,
    pub attempt_key: CommandAttemptKey,
    pub raw_output_artifact: RawOutputArtifact,
    pub shell_type: ShellType,
    pub shell_wrapper_is_owned: bool,
    pub hook_command: String,
    pub process_id: u32,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub cwd: PathUri,

    pub normalization_cwd: Option<PathBuf>,
    pub sandbox_cwd: PathUri,
    pub turn_environment: TurnEnvironment,
    pub network: Option<NetworkProxy>,
    pub tty: bool,
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub additional_permissions_uri: Option<UriAdditionalPermissionProfile>,
    pub additional_permissions_preapproved: bool,
    pub justification: Option<String>,
    pub prefix_rule: Option<Vec<String>>,
    pub validation_launch: Option<crate::validation_admission::ValidationLaunchPlan>,
    pub known_delta: Option<PreparedKnownDelta>,
}

/// Retains every process created by sandbox retries until startup is either
/// committed to the process store/ledger or cancelled and cleaned up.
#[derive(Clone, Default)]
pub(crate) struct PendingSpawnRegistration {
    processes: Arc<StdMutex<Vec<Arc<UnifiedExecProcess>>>>,
}

impl PendingSpawnRegistration {
    pub(crate) fn register(&self, process: Arc<UnifiedExecProcess>) {
        self.processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(process);
    }

    pub(crate) fn snapshot(&self) -> Vec<Arc<UnifiedExecProcess>> {
        self.processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn clear(&self) {
        self.processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

#[derive(Debug)]
pub(crate) struct WriteStdinRequest<'a> {
    pub process_id: u32,
    pub input: &'a str,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub truncation_policy: TruncationPolicy,
}

#[derive(Default)]
pub(crate) struct ProcessStore {
    processes: HashMap<u32, ProcessEntry>,
    reserved_process_ids: HashSet<u32>,
}

/// Owns a reserved process id until it is atomically transferred into the
/// process store. Dropping the transfer sender before then wakes the manager's
/// cleanup task, including when the calling future is cancelled.
pub(crate) struct ProcessIdReservation {
    process_id: u32,
    transfer_sender: Option<oneshot::Sender<()>>,
}

impl ProcessIdReservation {
    fn new(process_id: u32, transfer_sender: oneshot::Sender<()>) -> Self {
        Self {
            process_id,
            transfer_sender: Some(transfer_sender),
        }
    }

    pub(crate) fn process_id(&self) -> u32 {
        self.process_id
    }

    fn transfer_to_store(&mut self) {
        if let Some(sender) = self.transfer_sender.take() {
            let _ = sender.send(());
        }
    }
}

impl ProcessStore {
    fn remove(&mut self, process_id: u32) -> Option<ProcessEntry> {
        self.reserved_process_ids.remove(&process_id);
        self.processes.remove(&process_id)
    }
}

pub(crate) struct UnifiedExecProcessManager {
    process_store: Arc<Mutex<ProcessStore>>,
    max_write_stdin_yield_time_ms: u64,
    executor_ready_environments: StdMutex<HashSet<String>>,
}

impl UnifiedExecProcessManager {
    pub(crate) fn new(max_write_stdin_yield_time_ms: u64) -> Self {
        Self::new_with_deferred_executor(
            max_write_stdin_yield_time_ms,
            /*deferred_executor_enabled*/ false,
        )
    }

    pub(crate) fn new_with_deferred_executor(
        max_write_stdin_yield_time_ms: u64,
        _deferred_executor_enabled: bool,
    ) -> Self {
        Self {
            process_store: Arc::new(Mutex::new(ProcessStore::default())),
            max_write_stdin_yield_time_ms: max_write_stdin_yield_time_ms
                .max(MIN_EMPTY_YIELD_TIME_MS),
            executor_ready_environments: StdMutex::new(HashSet::new()),
        }
    }

    fn mark_executor_ready(&self, environment_id: &str) -> bool {
        let mut ready = self
            .executor_ready_environments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !ready.insert(environment_id.to_string())
    }
}

impl Default for UnifiedExecProcessManager {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS)
    }
}

struct ProcessEntry {
    process: Arc<UnifiedExecProcess>,
    command_execution_id: crate::tools::command_execution::CommandExecutionId,
    parent_tool_execution_id: codex_protocol::protocol::ToolExecutionId,
    call_id: String,
    process_id: u32,
    cwd: PathUri,
    initial_exec_command_active: Arc<std::sync::atomic::AtomicBool>,
    hook_command: String,
    tty: bool,
    network_approval: Option<DeferredNetworkApproval>,
    session: Weak<Session>,
    last_used: tokio::time::Instant,
}

#[cfg(test)]
pub(crate) fn clamp_yield_time(yield_time_ms: u64) -> u64 {
    clamp_yield_time_for_readiness(yield_time_ms, /*executor_ready*/ false)
}

pub(crate) fn clamp_yield_time_for_readiness(yield_time_ms: u64, executor_ready: bool) -> u64 {
    let yield_time_ms = if cfg!(windows) && !executor_ready {
        yield_time_ms.max(WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS)
    } else {
        yield_time_ms
    };
    yield_time_ms.clamp(MIN_YIELD_TIME_MS, MAX_YIELD_TIME_MS)
}

pub(crate) fn generate_chunk_id() -> String {
    let mut rng = rng();
    (0..6)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod process_tests;
#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
