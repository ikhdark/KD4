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
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;

use codex_network_proxy::NetworkProxy;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_tools::UnifiedExecShellMode;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_path_uri::PathUri;
use rand::Rng;
use rand::rng;
use tokio::sync::Mutex;

use crate::sandboxing::SandboxPermissions;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::network_approval::DeferredNetworkApproval;

mod async_watcher;
mod errors;
pub(crate) mod head_tail_buffer;
mod process;
mod process_manager;
mod process_state;

pub(crate) fn set_deterministic_process_ids_for_tests(enabled: bool) {
    process_manager::set_deterministic_process_ids_for_tests(enabled);
}

pub(crate) use errors::UnifiedExecError;
pub(crate) use process::NoopSpawnLifecycle;
#[cfg(unix)]
pub(crate) use process::SpawnLifecycle;
pub(crate) use process::SpawnLifecycleHandle;
pub(crate) use process::UnifiedExecProcess;

pub(crate) const MIN_YIELD_TIME_MS: u64 = 250;
pub(crate) const WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS: u64 = 2_000;
// Minimum yield time for an empty `write_stdin`.
pub(crate) const MIN_EMPTY_YIELD_TIME_MS: u64 = 5_000;
pub(crate) const MAX_YIELD_TIME_MS: u64 = 30_000;
pub(crate) const DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS: u64 = 300_000;
pub(crate) const DEFAULT_SUCCESS_OUTPUT_TOKENS: usize = 8_000;
pub(crate) const DEFAULT_FAILURE_OUTPUT_TOKENS: usize = 10_000;
pub(crate) const ADAPTIVE_DIAGNOSTIC_OUTPUT_TOKENS: usize = 10_000;
#[cfg(all(test, unix))]
pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: usize = DEFAULT_SUCCESS_OUTPUT_TOKENS;
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_BYTES: usize = 1024 * 1024; // 1 MiB
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_TOKENS: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES / 4;
pub(crate) const MAX_UNIFIED_EXEC_PROCESSES: usize = 64;

pub(crate) struct UnifiedExecContext {
    pub session: Arc<Session>,
    pub turn: Arc<TurnContext>,
    pub call_id: String,
    pub tracker: Option<SharedTurnDiffTracker>,
}

impl UnifiedExecContext {
    #[cfg(test)]
    pub fn new(session: Arc<Session>, turn: Arc<TurnContext>, call_id: String) -> Self {
        Self {
            session,
            turn,
            call_id,
            tracker: None,
        }
    }

    pub fn with_tracker(
        session: Arc<Session>,
        turn: Arc<TurnContext>,
        call_id: String,
        tracker: SharedTurnDiffTracker,
    ) -> Self {
        Self {
            session,
            turn,
            call_id,
            tracker: Some(tracker),
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
    pub hook_command: String,
    pub process_id: i32,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub cwd: PathUri,
    pub sandbox_cwd: PathUri,
    pub turn_environment: TurnEnvironment,
    pub shell_mode: UnifiedExecShellMode,
    pub network: Option<NetworkProxy>,
    pub tty: bool,
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub additional_permissions_preapproved: bool,
    pub justification: Option<String>,
    pub prefix_rule: Option<Vec<String>>,
}

#[derive(Debug)]
pub(crate) struct WriteStdinRequest<'a> {
    pub process_id: i32,
    pub input: &'a str,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub truncation_policy: TruncationPolicy,
}

#[derive(Default)]
pub(crate) struct ProcessStore {
    processes: HashMap<i32, ProcessEntry>,
    reserved_process_ids: HashSet<i32>,
}

impl ProcessStore {
    fn remove(&mut self, process_id: i32) -> Option<ProcessEntry> {
        self.reserved_process_ids.remove(&process_id);
        self.processes.remove(&process_id)
    }
}

pub(crate) struct UnifiedExecProcessManager {
    process_store: Mutex<ProcessStore>,
    max_write_stdin_yield_time_ms: u64,
    deferred_executor_enabled: bool,
    executor_ready: AtomicBool,
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
        deferred_executor_enabled: bool,
    ) -> Self {
        Self {
            process_store: Mutex::new(ProcessStore::default()),
            max_write_stdin_yield_time_ms: max_write_stdin_yield_time_ms
                .max(MIN_EMPTY_YIELD_TIME_MS),
            deferred_executor_enabled,
            executor_ready: AtomicBool::new(false),
        }
    }
}

impl Default for UnifiedExecProcessManager {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS)
    }
}

struct ProcessEntry {
    process: Arc<UnifiedExecProcess>,
    call_id: String,
    process_id: i32,
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
    let executor_ready = executor_ready && crate::latency_switches::stage5_executor_enabled();
    let yield_time_ms = if cfg!(windows) && !executor_ready {
        yield_time_ms.max(WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS)
    } else {
        yield_time_ms
    };
    yield_time_ms.clamp(MIN_YIELD_TIME_MS, MAX_YIELD_TIME_MS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputBudgetClass {
    Success,
    FailureOrTimeout,
}

pub(crate) fn resolve_adaptive_max_tokens(
    max_tokens: Option<usize>,
    class: OutputBudgetClass,
    command_text: Option<&str>,
    output_text: &str,
) -> usize {
    if let Some(max_tokens) = max_tokens {
        return max_tokens;
    }
    if !crate::latency_switches::stage4_output_budget_enabled() {
        return ADAPTIVE_DIAGNOSTIC_OUTPUT_TOKENS;
    }
    if is_high_signal_diagnostic(command_text, output_text) {
        return ADAPTIVE_DIAGNOSTIC_OUTPUT_TOKENS;
    }
    match class {
        OutputBudgetClass::Success => DEFAULT_SUCCESS_OUTPUT_TOKENS,
        OutputBudgetClass::FailureOrTimeout => DEFAULT_FAILURE_OUTPUT_TOKENS,
    }
}

fn is_high_signal_diagnostic(command_text: Option<&str>, output_text: &str) -> bool {
    let command = command_text.unwrap_or_default().to_ascii_lowercase();
    let diagnostic_command = [
        "cargo check",
        "cargo test",
        "cargo nextest",
        "cargo clippy",
        "rustc ",
        "pytest",
        "python -m unittest",
        "npm test",
        "npm run test",
        "pnpm test",
        "yarn test",
        "dotnet test",
        "go test",
        "just test",
        "just check",
    ]
    .iter()
    .any(|needle| command.contains(needle));
    if diagnostic_command {
        return true;
    }

    let output = output_text.to_ascii_lowercase();
    [
        "stack backtrace:",
        "traceback (most recent call last):",
        "thread 'main' panicked at",
        "error[e",
        "test result: failed",
        "failures:",
        "compiler error",
        "caused by:",
    ]
    .iter()
    .any(|needle| output.contains(needle))
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
#[cfg(unix)]
#[path = "mod_tests.rs"]
mod tests;
