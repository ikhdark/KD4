use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::codex_thread::BackgroundTerminalInfo;
use crate::exec_env::CODEX_PERMISSION_PROFILE_ENV_VAR;
use crate::exec_env::exec_env_policy_from_shell_policy;
use crate::exec_env::CODEX_THREAD_ID_ENV_VAR;
use crate::exec_env::create_env;
use crate::exec_env::inject_permission_profile_env;
use crate::exec_policy::ExecApprovalRequest;
use crate::sandboxing::ExecOptions;
use crate::sandboxing::ExecRequest;
use crate::sandboxing::ExecServerEnvConfig;
use crate::session::BackgroundCommandAppendReceipt;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolCallSource;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::runtimes::unified_exec::UnifiedExecRequest as UnifiedExecToolRequest;
use crate::tools::runtimes::unified_exec::UnifiedExecRuntime;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;

use crate::tools::sandboxing::same_exec_authorization_envelope;
use crate::tools::tool_dispatch_trace::ToolDispatchTiming;
use crate::tools::tool_dispatch_trace::active_tool_dispatch_timing;
use crate::tools::tool_dispatch_trace::mark_exec_process_exited;
use crate::tools::tool_dispatch_trace::mark_exec_process_spawned;
use crate::tools::tool_dispatch_trace::scope_tool_dispatch_timing;
use crate::turn_timing::TurnLocalPhase;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES;
use crate::unified_exec::MAX_YIELD_TIME_MS;
use crate::unified_exec::MIN_YIELD_TIME_MS;
use crate::unified_exec::PendingSpawnRegistration;
use crate::unified_exec::ProcessEntry;
use crate::unified_exec::ProcessIdReservation;
use crate::unified_exec::ProcessStore;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::WriteStdinObservation;
use crate::unified_exec::WriteStdinRequest;
use crate::unified_exec::async_watcher::emit_exec_end_for_unified_exec;
use crate::unified_exec::async_watcher::lagged_output_marker;
use crate::unified_exec::async_watcher::spawn_exit_watcher;
use crate::unified_exec::async_watcher::start_streaming_output;
use crate::unified_exec::async_watcher::wait_for_process_output_drain;
use crate::unified_exec::clamp_initial_yield_time;
use crate::unified_exec::cold_start_yield_allowance;
use crate::unified_exec::generate_chunk_id;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::OutputBuffer;
use crate::unified_exec::process::OutputHandles;
use crate::unified_exec::process::SpawnLifecycleHandle;
use crate::unified_exec::process::UnifiedExecProcess;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::config_types::ShellEnvironmentPolicyInherit;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::NextSampleBlockReason;
use codex_protocol::protocol::ToolLifecycleTimerWait;
use codex_protocol::protocol::ToolLifecycleWakeReason;
use codex_sandboxing::SandboxCommand;

use crate::tools::runtimes::prove_noprofile_powershell_direct_argv_async;
use codex_tools::ToolName;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_path_uri::PathUri;

const UNIFIED_EXEC_ENV: [(&str, &str); 11] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("LANG", "C.UTF-8"),
    ("LC_CTYPE", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    // Python ignores the locale variables on Windows and encodes piped stdio
    // with the ANSI code page; characters outside it raise UnicodeEncodeError.
    ("PYTHONUTF8", "1"),
    ("COLORTERM", ""),
    ("PAGER", UNIFIED_EXEC_PAGER),
    ("GIT_PAGER", UNIFIED_EXEC_PAGER),
    ("GH_PAGER", UNIFIED_EXEC_PAGER),
    ("CODEX_CI", "1"),
];

#[cfg(windows)]
// `more.com` interprets text and rewrites line endings. Copy the byte stream so
// pager-using commands preserve the same output as the Unix `cat` default.
// Encode `[Console]::OpenStandardInput().CopyTo([Console]::OpenStandardOutput())`
// as UTF-16LE Base64 to avoid different quoting rules in cmd.exe and Git's sh.
const UNIFIED_EXEC_PAGER: &str = "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand WwBDAG8AbgBzAG8AbABlAF0AOgA6AE8AcABlAG4AUwB0AGEAbgBkAGEAcgBkAEkAbgBwAHUAdAAoACkALgBDAG8AcAB5AFQAbwAoAFsAQwBvAG4AcwBvAGwAZQBdADoAOgBPAHAAZQBuAFMAdABhAG4AZABhAHIAZABPAHUAdABwAHUAdAAoACkAKQA=";
#[cfg(not(windows))]
const UNIFIED_EXEC_PAGER: &str = "cat";
const INITIAL_OUTPUT_QUIET_PERIOD: Duration = Duration::from_millis(250);
const INTERRUPT: &str = "\u{3}";
// Failure confirmation can take longer than a foreground tool can wait. The
// session terminal tracker keeps that work owned after a structured handback.
const FAILURE_HANDBACK_GRACE: Duration = Duration::from_secs(2);
/// Headroom reserved inside a nested call's budget for returning the result
/// through dispatch and transport to the runtime wrapper.
///
/// Observation stops this early so the wrapper receives a yielded process
/// handle instead of dropping the call at its own hard deadline.
pub(crate) const NESTED_POLL_MARGIN: Duration = Duration::from_millis(2_000);

/// Latest instant this call may still be observing output, given the runtime
/// wrapper's hard deadline. `None` when no wrapper bounds the call.
///
/// Never earlier than `now`: an already-exhausted budget yields immediately
/// rather than producing a deadline in the past that reads as an error.
pub(crate) fn nested_poll_bound(nested_deadline: Option<std::time::Instant>) -> Option<Instant> {
    let nested_deadline = Instant::from_std(nested_deadline?);
    let now = Instant::now();
    Some(
        nested_deadline
            .checked_sub(NESTED_POLL_MARGIN)
            .unwrap_or(now)
            .max(now),
    )
}

struct StdinMutation<'a> {
    cancellation: &'a CancellationToken,
    started: bool,
}

/// Tokio's timeout polls its inner future first. Keep its timer wakeup, but
/// check the absolute bound before each poll of an effect-bearing operation.
async fn poll_stdin_before<F: std::future::Future>(
    deadline: Option<Instant>,
    cancellation: Option<&CancellationToken>,
    operation: F,
) -> Option<F::Output> {
    let mut operation = std::pin::pin!(operation);
    let guarded = std::future::poll_fn(|cx| {
        if deadline.is_some_and(|bound| Instant::now() >= bound)
            || cancellation.is_some_and(CancellationToken::is_cancelled)
        {
            std::task::Poll::Ready(None)
        } else {
            operation.as_mut().poll(cx).map(Some)
        }
    });
    let wait = async {
        match deadline {
            Some(bound) => tokio::time::timeout_at(bound, guarded).await.ok().flatten(),
            None => guarded.await,
        }
    };
    if let Some(cancellation) = cancellation {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => None,
            result = wait => result,
        }
    } else {
        wait.await
    }
}

struct CollectedOutput {
    bytes: Vec<u8>,
    stream_ranges: Option<[std::ops::Range<u64>; 2]>,
    ranges: Option<crate::unified_exec::head_tail_buffer::OutputChunkRanges>,
    wake_reason: ToolLifecycleWakeReason,
    observation_deadline: Instant,
}

// Record the manager's actual deadline, including failures before collection.
struct WriteStdinWait(ToolLifecycleTimerWait);

impl Drop for WriteStdinWait {
    fn drop(&mut self) {
        if let Some(timing) = active_tool_dispatch_timing() {
            timing.record_timer_wait(self.0.clone());
        }
    }
}

fn apply_unified_exec_env(
    mut env: HashMap<String, String>,
    policy: &ShellEnvironmentPolicy,
) -> HashMap<String, String> {
    if policy.inherit != ShellEnvironmentPolicyInherit::All {
        return env;
    }

    for (key, value) in UNIFIED_EXEC_ENV {
        if env
            .keys()
            .any(|existing_key| existing_key.eq_ignore_ascii_case(key))
            || policy.exclude.iter().any(|pattern| pattern.matches(key))
            || (!policy.include_only.is_empty()
                && !policy
                    .include_only
                    .iter()
                    .any(|pattern| pattern.matches(key)))
        {
            continue;
        }
        env.insert(key.to_string(), value.to_string());
    }
    env
}

fn build_unified_exec_environment(
    context: &UnifiedExecContext,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut local_policy_env = create_env(
        &context.turn.config.permissions.shell_environment_policy,
        /*thread_id*/ None,
    );
    context.apply_env_overrides(&mut local_policy_env);
    let mut env = local_policy_env.clone();
    if cfg!(windows) {
        env.retain(|name, _| !name.eq_ignore_ascii_case(CODEX_THREAD_ID_ENV_VAR));
    }
    env.insert(
        CODEX_THREAD_ID_ENV_VAR.to_string(),
        context.session.thread_id.to_string(),
    );
    let active_permission_profile = context.turn.config.permissions.active_permission_profile();
    inject_permission_profile_env(&mut env, active_permission_profile.as_ref());
    (
        apply_unified_exec_env(
            env,
            &context.turn.config.permissions.shell_environment_policy,
        ),
        local_policy_env,
    )
}

fn env_overlay_for_exec_server(
    request_env: &HashMap<String, String>,
    local_policy_env: &HashMap<String, String>,
) -> HashMap<String, String> {
    request_env
        .iter()
        .filter(|(key, value)| {
            key.as_str() == CODEX_PERMISSION_PROFILE_ENV_VAR
                || local_policy_env.get(*key) != Some(*value)
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}


async fn exec_server_env_for_request(
    request: &ExecRequest,
) -> Result<
    (
        Option<codex_exec_server::ExecEnvPolicy>,
        HashMap<String, String>,
    ),
    UnifiedExecError,
> {
    let request_env = request.env.clone();
    let env_config = request.exec_server_env_config.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(exec_server_env_config) = &env_config {
            let env =
                env_overlay_for_exec_server(&request_env, &exec_server_env_config.local_policy_env);
            let policy = exec_server_env_config.policy.clone();
            (Some(policy), env)
        } else {
            (None, request_env)
        }
    })
    .await
    .map_err(|err| {
        UnifiedExecError::create_process(format!("exec-server environment worker failed: {err}"))
    })
}

async fn exec_server_params_for_request(
    process_id: u32,
    request: &ExecRequest,
    tty: bool,
    pipe_stdin: bool,
) -> Result<codex_exec_server::ExecParams, UnifiedExecError> {
    let (env_policy, env) = exec_server_env_for_request(request).await?;
    // Public handles are manager-local; a shared executor needs a fresh ID for
    // every start, including unsandboxed requests and sandbox retries.
    let exec_server_process_id = format!("{process_id}-{}", Uuid::new_v4());
    Ok(codex_exec_server::ExecParams {
        process_id: exec_server_process_id.into(),
        argv: request.command.clone(),
        cwd: request.cwd.clone(),
        env_policy,
        env,
        tty,
        pipe_stdin,
        arg0: request.arg0.clone(),
        sandbox: None,
        enforce_managed_network: false,
        managed_network: None,
    })
}

/// Borrowed process state prepared for a `write_stdin` or poll operation.
struct PreparedProcessHandles {
    process: Arc<UnifiedExecProcess>,
    output_buffer: OutputBuffer,
    output_notify: Arc<Notify>,
    output_closed: Arc<AtomicBool>,
    output_closed_notify: Arc<Notify>,
    cancellation_token: CancellationToken,
    pause_state: Option<watch::Receiver<bool>>,
    call_id: String,
    hook_command: String,
    hook_cwd: Option<PathUri>,
    process_id: u32,
    search_exit_one_is_no_match: bool,
    tty: bool,
    validation_launch: bool,
    terminal_tasks: Option<tokio_util::task::TaskTracker>,
}

struct FailureOutputContext {
    call_id: String,
    hook_command: String,
    hook_cwd: Option<PathUri>,
    tty: bool,
}

impl FailureOutputContext {
    fn for_exec(context: &UnifiedExecContext, request: &ExecCommandRequest) -> Self {
        Self {
            call_id: context.call_id.clone(),
            hook_command: request.hook_command.clone(),
            hook_cwd: (!request.turn_environment.environment.is_remote())
                .then(|| request.cwd.clone()),
            tty: request.tty,
        }
    }

    fn for_stdin(handles: &PreparedProcessHandles) -> Self {
        Self {
            call_id: handles.call_id.clone(),
            hook_command: handles.hook_command.clone(),
            hook_cwd: handles.hook_cwd.clone(),
            tty: handles.tty,
        }
    }
}

struct InitialExecCommandGuard {
    active: Arc<AtomicBool>,
}

impl Drop for InitialExecCommandGuard {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

pub(super) struct PendingProcessRegistration {
    process_store: Arc<tokio::sync::Mutex<ProcessStore>>,
    session: Arc<crate::session::session::Session>,
    attempt_key: crate::tools::command_execution::CommandAttemptKey,
    process_id: u32,
    pending_spawns: PendingSpawnRegistration,
    cleanup_owners: Arc<PendingProcessCleanupOwners>,
    primary_process: Option<Arc<UnifiedExecProcess>>,
    initial_exec_command_active: Option<Arc<AtomicBool>>,
    pending_startup_completion: Option<PendingStartupCompletion>,
    runtime: tokio::runtime::Handle,
    observation_deadline: Option<Instant>,
    committed: bool,
}

enum StartupBegin {
    LiveOnly,
    Appending(tokio::task::JoinHandle<std::io::Result<()>>),
    Accepted,
    Unconfirmed { observed: bool, message: String },
}

struct PendingStartupCompletion {
    begin: StartupBegin,
    turn: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
    started_at: Instant,
    tty: bool,
    source: ToolCallSource,
    tracker: Option<SharedTurnDiffTracker>,
    timing: Option<Arc<ToolDispatchTiming>>,
}

impl PendingStartupCompletion {
    async fn observe_begin_append(&mut self) {
        if let StartupBegin::Appending(append) = &mut self.begin {
            self.begin = match append.await {
                Ok(Ok(())) => StartupBegin::Accepted,
                Ok(Err(error)) => StartupBegin::Unconfirmed {
                    observed: true,
                    message: format!("startup Begin rollout append failed: {error}"),
                },
                Err(error) => StartupBegin::Unconfirmed {
                    observed: false,
                    message: format!("startup Begin rollout append outcome is unknown: {error}"),
                },
            };
        }
    }

    async fn finish_abandoned(
        mut self,
        session: Arc<Session>,
        process: Arc<UnifiedExecProcess>,
        append: BackgroundTerminalAppend,
    ) {
        self.observe_begin_append().await;
        match self.begin {
            StartupBegin::Accepted => {}
            StartupBegin::Unconfirmed { observed, message } => {
                append.settle(Some((observed, message)));
                return;
            }
            StartupBegin::LiveOnly | StartupBegin::Appending(_) => return,
        }
        let record_append = |receipt| append.record(receipt);
        let emit = emit_exec_end_for_unified_exec(
            session,
            self.turn,
            self.call_id,
            self.command,
            self.cwd,
            Some(append.process_id.to_string()),
            self.transcript,
            String::new(),
            Some(process.as_ref()),
            self.tty,
            false,
            Some("unified exec startup was abandoned before registration".to_string()),
            false,
            self.started_at.elapsed(),
            self.source,
            self.tracker,
            None,
            Some(&record_append),
        );
        // Preserve the original Begin's dispatch identity without taking on
        // the promoted watcher's command-execution ledger responsibilities.
        let result = match self.timing {
            Some(timing) => scope_tool_dispatch_timing(timing, emit).await,
            None => emit.await,
        };
        if let Err(error) = result {
            append.record_unknown(error.to_string());
        }
    }
}

pub(super) type PendingProcessCleanupOwners = std::sync::Mutex<Vec<PendingProcessOwner>>;

pub(super) enum PendingProcessOwner {
    Startup(Arc<PendingProcessCleanup>),
    BackgroundAppend(BackgroundAppendPending),
    BackgroundAppendFailure {
        owner: TerminalAppendOwner,
        was_observed: bool,
        message: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalAppendOwner {
    ExitWatcher,
    AbandonedStartup,
}

pub(super) struct BackgroundAppendPending {
    process_id: u32,
    incarnation: Uuid,
    owner: TerminalAppendOwner,
    native_termination_confirmed: Option<tokio::sync::oneshot::Sender<()>>,
    settled: CancellationToken,
}

/// Owns the exact watched terminal even after its poll/termination entry is removed.
pub(super) struct BackgroundTerminalAppend {
    owners: std::sync::Weak<PendingProcessCleanupOwners>,
    process_id: u32,
    incarnation: Uuid,
    owner: TerminalAppendOwner,
    native_termination: tokio::sync::oneshot::Receiver<()>,
    settled: CancellationToken,
}

impl BackgroundTerminalAppend {
    fn enroll(
        owners: &Arc<PendingProcessCleanupOwners>,
        process_id: u32,
        incarnation: Uuid,
    ) -> Result<Self, UnifiedExecError> {
        let mut retained = owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if retained
            .iter()
            .filter(|owner| matches!(owner, PendingProcessOwner::BackgroundAppend(pending)
                if pending.owner == TerminalAppendOwner::ExitWatcher))
            .count()
            >= MAX_UNIFIED_EXEC_PROCESSES
        {
            // Bound even watchers whose process was removed while their
            // live send is blocked. Writer retirement only waits for append.
            return Err(UnifiedExecError::process_failed(
                "background command terminal finalizers are still pending".to_string(),
            ));
        }
        Ok(Self::register(
            owners,
            &mut retained,
            process_id,
            incarnation,
            TerminalAppendOwner::ExitWatcher,
        ))
    }

    fn register(
        owners: &Arc<PendingProcessCleanupOwners>,
        retained: &mut Vec<PendingProcessOwner>,
        process_id: u32,
        incarnation: Uuid,
        owner: TerminalAppendOwner,
    ) -> Self {
        let (confirmed, native_termination) = tokio::sync::oneshot::channel();
        let settled = CancellationToken::new();
        retained.push(PendingProcessOwner::BackgroundAppend(BackgroundAppendPending {
            process_id,
            incarnation,
            owner,
            native_termination_confirmed: (owner == TerminalAppendOwner::ExitWatcher)
                .then_some(confirmed),
            settled: settled.clone(),
        }));
        Self {
            owners: Arc::downgrade(owners),
            process_id,
            incarnation,
            owner,
            native_termination,
            settled,
        }
    }

    pub(super) async fn wait_for_confirmed_termination(&mut self) -> bool {
        (&mut self.native_termination).await.is_ok()
    }

    pub(super) fn record(&self, receipt: BackgroundCommandAppendReceipt) {
        let failure = match receipt {
            BackgroundCommandAppendReceipt::Accepted | BackgroundCommandAppendReceipt::NotRequired => None,
            BackgroundCommandAppendReceipt::Failed(error) => Some((true, error)),
        };
        self.settle(failure);
    }

    pub(super) fn record_unknown(&self, reason: String) {
        self.settle(Some((false, reason)));
    }

    fn settle(&self, failure: Option<(bool, String)>) {
        let Some(owners) = self.owners.upgrade() else {
            return;
        };
        let mut retained = owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.settled.is_cancelled() {
            return;
        }
        if let Some((was_observed, error)) = failure {
            let process_id = self.process_id;
            let incarnation = self.incarnation;
            let phase = match self.owner {
                TerminalAppendOwner::ExitWatcher => "terminal rollout append",
                TerminalAppendOwner::AbandonedStartup => "startup command rollout custody",
            };
            let outcome = if was_observed { "failed" } else { "is unknown" };
            let message = format!("process {process_id} ({incarnation}) {phase} {outcome}: {error}");
            // Keep one example per owner and failure class for the shutdown
            // result. Log each abandoned startup's exact failure separately.
            let first = !retained.iter().any(|entry| matches!(
                entry,
                PendingProcessOwner::BackgroundAppendFailure { owner, was_observed: previous, .. }
                    if *owner == self.owner && *previous == was_observed
            ));
            if first || self.owner == TerminalAppendOwner::AbandonedStartup {
                tracing::warn!(%message);
            }
            if first {
                retained.push(PendingProcessOwner::BackgroundAppendFailure {
                    owner: self.owner,
                    was_observed,
                    message,
                });
            }
        }
        self.settled.cancel();
    }
}

impl Drop for BackgroundTerminalAppend {
    fn drop(&mut self) {
        if !self.settled.is_cancelled() {
            let reason = match self.owner {
                TerminalAppendOwner::ExitWatcher => "exit watcher ended without an append receipt",
                TerminalAppendOwner::AbandonedStartup => {
                    "abandoned startup finalizer ended without an append receipt"
                }
            };
            self.record_unknown(reason.to_string());
        }
        let Some(owners) = self.owners.upgrade() else {
            return;
        };
        owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|owner| !matches!(
                owner,
                PendingProcessOwner::BackgroundAppend(pending)
                    if pending.process_id == self.process_id
                        && pending.incarnation == self.incarnation
                        && pending.owner == self.owner
            ));
    }
}

fn note_background_termination(
    owners: &PendingProcessCleanupOwners,
    process_id: u32,
    process: &UnifiedExecProcess,
) {
    let incarnation = process.incarnation();
    let mut retained = owners
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(confirmed) = retained.iter_mut().find_map(|owner| match owner {
        PendingProcessOwner::BackgroundAppend(pending)
            if pending.process_id == process_id
                && pending.incarnation == incarnation
                && pending.owner == TerminalAppendOwner::ExitWatcher =>
        {
            pending.native_termination_confirmed.take()
        }
        _ => None,
    }) {
        let _ = confirmed.send(());
    }
}

#[derive(Clone)]
pub(super) struct PendingProcessCleanup {
    process_store: std::sync::Weak<tokio::sync::Mutex<ProcessStore>>,
    session: std::sync::Weak<crate::session::session::Session>,
    cleanup_owners: std::sync::Weak<PendingProcessCleanupOwners>,
    reconciliation_started: Arc<AtomicBool>,
    attempt_key: crate::tools::command_execution::CommandAttemptKey,
    process_id: u32,
    processes: Vec<PendingProcessToTerminate>,
    primary_process: Option<Arc<UnifiedExecProcess>>,
}

#[derive(Clone)]
struct PendingProcessToTerminate {
    process: Arc<UnifiedExecProcess>,
    requires_confirmed_termination: bool,
}

impl PendingProcessRegistration {
    pub(super) fn new(
        process_store: Arc<tokio::sync::Mutex<ProcessStore>>,
        context: &UnifiedExecContext,
        attempt_key: crate::tools::command_execution::CommandAttemptKey,
        process_id: u32,
    ) -> Self {
        Self {
            process_store,
            session: Arc::clone(&context.session),
            attempt_key,
            process_id,
            pending_spawns: PendingSpawnRegistration::with_termination_owner(
                crate::unified_exec::process::ProcessTerminationOwner {
                    tasks: context.session.terminal_tasks.clone(),
                    runtime: tokio::runtime::Handle::current(),
                },
            ),
            cleanup_owners: Arc::clone(
                &context
                    .session
                    .services
                    .unified_exec_manager
                    .pending_cleanup_owners,
            ),
            primary_process: None,
            initial_exec_command_active: None,
            pending_startup_completion: None,
            runtime: tokio::runtime::Handle::current(),
            observation_deadline: None,
            committed: false,
        }
    }

    pub(super) fn pending_spawns(&self) -> PendingSpawnRegistration {
        self.pending_spawns.clone()
    }

    pub(super) fn attach_process(
        &mut self,
        process: Arc<UnifiedExecProcess>,
        ) {
        assert!(
            !self.committed,
            "cannot attach a process after registration is committed"
        );
        assert!(
            self.primary_process.is_none(),
            "cannot attach more than one primary process to a registration"
        );
        self.primary_process = Some(process);
    }

    pub(super) fn set_initial_exec_command_active(&mut self, active: Arc<AtomicBool>) {
        assert!(
            !self.committed,
            "cannot attach command activity after registration is committed"
        );
        assert!(
            self.initial_exec_command_active.is_none(),
            "cannot attach command activity more than once"
        );
        self.initial_exec_command_active = Some(active);
    }

    fn cleanup_payload(&self) -> PendingProcessCleanup {
        let mut processes = self.pending_spawns.snapshot();
        if let Some(primary_process) = self.primary_process.as_ref()
            && !processes
                .iter()
                .any(|process| Arc::ptr_eq(process, primary_process))
        {
            processes.push(Arc::clone(primary_process));
        }
        let processes = processes
            .into_iter()
            .map(|process| PendingProcessToTerminate {
                // A dead root can leave inherited output pipes open before the
                // streaming watcher takes custody. Confirmed termination also
                // releases those readers; root exit alone cannot release admission.
                requires_confirmed_termination: !process.has_exited()
                    || !process.output_is_closed(),
                process,
            })
            .collect();
        PendingProcessCleanup {
            process_store: Arc::downgrade(&self.process_store),
            session: Arc::downgrade(&self.session),
            cleanup_owners: Arc::downgrade(&self.cleanup_owners),
            reconciliation_started: Arc::new(AtomicBool::new(false)),
            attempt_key: self.attempt_key.clone(),
            process_id: self.process_id,
            processes,
            primary_process: self.primary_process.clone(),
        }
    }

    fn retain_cleanup(&self, confirm_primary_termination: bool) -> Arc<PendingProcessCleanup> {
        let mut cleanup = self.cleanup_payload();
        if confirm_primary_termination
            && let Some(primary) = cleanup.primary_process.as_ref()
        {
            for pending in &mut cleanup.processes {
                if Arc::ptr_eq(primary, &pending.process) {
                    pending.requires_confirmed_termination = true;
                }
            }
        }
        let cleanup = Arc::new(cleanup);
        // Retain custody before scheduling; session shutdown can find this exact
        // payload even if the runtime no longer runs its original task.
        self.cleanup_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(PendingProcessOwner::Startup(Arc::clone(&cleanup)));
        cleanup
    }

    fn register_startup_append(
        &self,
        owners: &mut Vec<PendingProcessOwner>,
        process: &UnifiedExecProcess,
    ) -> BackgroundTerminalAppend {
        // Admission already happened. Its exact debt must survive even if
        // the separate, bounded promoted-watcher population is full.
        BackgroundTerminalAppend::register(
            &self.cleanup_owners,
            owners,
            self.process_id,
            process.incarnation(),
            TerminalAppendOwner::AbandonedStartup,
        )
    }

    async fn cleanup(&mut self) -> Result<(), String> {
        assert!(!self.committed, "cannot clean up a committed registration");
        if let Some(active) = self.initial_exec_command_active.as_ref() {
            active.store(false, Ordering::Release);
        }
        cleanup_pending_process_registration(&self.cleanup_payload()).await?;
        self.committed = true;
        self.pending_spawns.clear().await;
        Ok(())
    }

    async fn commit(&mut self) {
        assert!(
            !self.committed,
            "cannot commit a registration more than once"
        );
        assert!(
            self.primary_process.is_some(),
            "cannot commit before the primary process is attached"
        );
        self.committed = true;
        self.pending_spawns.clear().await;
    }
}

impl Drop for PendingProcessRegistration {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(active) = self.initial_exec_command_active.as_ref() {
            active.store(false, Ordering::Release);
        }
        let startup = self
            .pending_startup_completion
            .take()
            .filter(|startup| !matches!(&startup.begin, StartupBegin::LiveOnly));
        let cleanup = Arc::new(self.cleanup_payload());
        let abandoned = {
            // Publish both debts together, before either owner can be
            // reconciled by shutdown or schedule an asynchronous operation.
            let mut owners = self
                .cleanup_owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            owners.push(PendingProcessOwner::Startup(Arc::clone(&cleanup)));
            startup.map(|startup| {
                let process = cleanup
                    .primary_process
                    .as_ref()
                    .expect("command Begin follows attached process");
                let append = self.register_startup_append(&mut owners, process);
                (startup, Arc::clone(process), append)
            })
        };
        if let Some((startup, process, append)) = abandoned {
            // Writer retirement waits for the append receipt, not the live
            // channels or the completion of this detached task.
            self.runtime.spawn(startup.finish_abandoned(
                Arc::clone(&self.session),
                process,
                append,
            ));
        }
        if tokio::runtime::Handle::try_current().is_ok() {
            self.session
                .terminal_tasks
                .spawn(run_pending_process_cleanup(cleanup));
        } else {
            enqueue_pending_process_cleanup(cleanup);
        }
    }
}

fn pending_process_cleanup_sender()
-> Option<tokio::sync::mpsc::UnboundedSender<Arc<PendingProcessCleanup>>> {
    static SENDER: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedSender<Arc<PendingProcessCleanup>>>,
    > = std::sync::Mutex::new(None);
    let mut cached = SENDER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(sender) = cached.as_ref().filter(|sender| !sender.is_closed()) {
        return Some(sender.clone());
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "failed to build unified exec cleanup runtime");
            return None;
        }
    };
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<Arc<PendingProcessCleanup>>();
    match std::thread::Builder::new()
        .name("codex-unified-exec-cleanup".to_string())
        .spawn(move || runtime.block_on(drain_pending_process_cleanups(receiver)))
    {
        Ok(_handle) => {
            *cached = Some(sender.clone());
            Some(sender)
        }
        Err(error) => {
            tracing::error!(%error, "failed to spawn unified exec cleanup worker");
            None
        }
    }
}

fn enqueue_pending_process_cleanup(cleanup: Arc<PendingProcessCleanup>) {
    // A failed bootstrap must retain the work, not just allow the next caller
    // to retry creating a worker. Only this lock drains the retry backlog.
    static BACKLOG: std::sync::Mutex<Vec<Arc<PendingProcessCleanup>>> =
        std::sync::Mutex::new(Vec::new());
    let mut backlog = BACKLOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    backlog.push(cleanup);
    if let Some(sender) = pending_process_cleanup_sender() {
        while let Some(cleanup) = backlog.pop() {
            if let Err(error) = sender.send(cleanup) {
                backlog.push(error.0);
                break;
            }
        }
    }
}

async fn drain_pending_process_cleanups(
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<Arc<PendingProcessCleanup>>,
) {
    let mut tasks = tokio::task::JoinSet::new();
    let mut closed = false;
    loop {
        tokio::select! {
            cleanup = receiver.recv(), if !closed && tasks.len() < 8 => match cleanup {
                Some(cleanup) => { tasks.spawn(run_pending_process_cleanup(cleanup)); }
                None => closed = true,
            },
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result {
                    tracing::error!(%error, "process cleanup task failed; custody remains retained");
                }
            }
            else => break,
        }
    }
}

async fn run_pending_process_cleanup(cleanup: Arc<PendingProcessCleanup>) {
    // Shutdown may discover the same retained payload as the Drop-owned runner.
    // Only one runner may reconcile/release its reservation, including after reuse.
    if cleanup.reconciliation_started.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut backoff = Duration::from_millis(100);
    loop {
        match cleanup_pending_process_registration(&cleanup).await {
        Ok(()) => {
            if let Some(owners) = cleanup.cleanup_owners.upgrade() {
                owners
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|owner| !matches!(
                        owner,
                        PendingProcessOwner::Startup(pending) if Arc::ptr_eq(pending, &cleanup)
                    ));
            }
            return;
        }
        Err(error) => {
            tracing::error!(%error, process_id = cleanup.process_id,
                retry_after_ms = backoff.as_millis(),
                "retaining cancelled unified exec startup for cleanup reconciliation");
            // The retained owner and exact process identities survive each retry.
            // No process-store guard is held while terminating or sleeping.
            tokio::time::sleep(backoff).await;
            backoff = backoff.saturating_mul(2).min(Duration::from_secs(5));
        }
        }
    }
}

async fn cleanup_pending_process_registration(
    cleanup: &PendingProcessCleanup,
) -> Result<(), String> {
    let results = futures::future::join_all(
        cleanup
            .processes
            .iter()
            .filter(|pending| pending.requires_confirmed_termination)
            .map(|pending| pending.process.terminate_confirmed()),
    )
    .await;
    if let Some(error) = results.into_iter().find_map(Result::err) {
        return Err(format!("process termination was not confirmed: {error}"));
    }

    let (removed_entry, process_id_conflict) = if let Some(store) = cleanup.process_store.upgrade()
    {
        let mut store = store.lock().await;
        match (
            store.processes.get(&cleanup.process_id),
            cleanup.primary_process.as_ref(),
        ) {
            (Some(entry), Some(primary_process))
                if Arc::ptr_eq(&entry.process, primary_process) =>
            {
                (store.remove(cleanup.process_id), false)
            }
            (Some(_), _) => (None, true),
            (None, _) => {
                // Startup custody, rather than the caller future, owns this
                // reservation until every spawned process is confirmed gone.
                store.release_reservation(cleanup.process_id);
                (None, false)
            }
        }
    } else {
        (None, false)
    };
    let session = cleanup.session.upgrade();
    drop(removed_entry);

    let Some(session) = session else {
        return Ok(());
    };
    let running = session
        .services
        .command_execution
        .running_process(cleanup.process_id)
        .await;
    if !process_id_conflict
        && let Some(running) = running.as_ref()
        && running.key == cleanup.attempt_key
    {
        let exit_code = cleanup
            .primary_process
            .as_ref()
            .and_then(|process| process.exit_code());
        session
            .services
            .command_execution
            .finish_running_process_with_execution_id(
                cleanup.process_id,
                running.execution_id,
                &running.parent_tool_execution_id,
                exit_code,
            )
            .await;
    }
    Ok(())
}

// Keep a short command on the direct path: there is no live session to poll,
// but its already-accepted Begin still has exactly one End owner. Start that
// owner before waiting, so timing out cannot interrupt event persistence.
#[allow(clippy::too_many_arguments)]
fn start_direct_completion(
    registration: &mut PendingProcessRegistration,
    context: &UnifiedExecContext,
    request: &ExecCommandRequest,
    cwd: PathUri,
    transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
    fallback_output: String,
    classified_search: bool,
    failure: Option<String>,
    wall_time: Duration,
    drain_relay: bool,
) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
    let process = registration
        .primary_process
        .as_ref()
        .expect("completion follows an attached process and accepted command Begin")
        .clone();
    let cleanup = registration.retain_cleanup(failure.is_some());
    registration.committed = true;
    let session = Arc::clone(&context.session);
    let turn = Arc::clone(&context.turn);
    let call_id = context.call_id.clone();
    let command = request.command_for_safety.clone();
    let process_id = request.process_id.to_string();
    let tty = request.tty;
    let source = context.source.clone();
    let tracker = context.tracker.clone();
    let timing = active_tool_dispatch_timing();
    let (completed, completion) = tokio::sync::oneshot::channel();
    context.session.terminal_tasks.spawn(async move {
        if drain_relay {
            let started_at = Instant::now();
            wait_for_process_output_drain(&process.output_drained_token()).await;
            if let Some(timing) = timing.as_ref() {
                timing.record_timer_wait(ToolLifecycleTimerWait {
                    wait_kind: "initial_process_output_drain".to_string(),
                    effective_timeout_ms: Some(
                        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                    ),
                    wake_reason: ToolLifecycleWakeReason::Completed,
                    ..Default::default()
                });
            }
        }
        let emit = emit_exec_end_for_unified_exec(
            Arc::clone(&session),
            Arc::clone(&turn),
            call_id,
            command,
            cwd,
            Some(process_id),
            transcript,
            fallback_output,
            Some(process.as_ref()),
            tty,
            classified_search,
            failure,
            false,
            wall_time,
            source,
            tracker,
            None,
            None,
        );
        // Preserve the accepted Begin's dispatch execution identity. Passing it
        // as a registered watcher execution id would incorrectly touch the ledger.
        let result = match timing {
            Some(timing) => scope_tool_dispatch_timing(timing, emit).await,
            None => emit.await,
        }
        .map_err(|error| error.to_string());
        if let Err(Err(message)) = completed.send(result) {
            // A caller that already yielded cannot report a late durability
            // failure. The session remains the owner of that failure.
            session
                .send_event(
                    turn.as_ref(),
                    codex_protocol::protocol::EventMsg::Error(
                        codex_protocol::protocol::ErrorEvent {
                            message,
                            codex_error_info: Some(codex_protocol::protocol::CodexErrorInfo::Other),
                        },
                    ),
                )
                .await;
        }
        run_pending_process_cleanup(cleanup).await;
    });
    completion
}

async fn wait_for_direct_completion(
    completion: tokio::sync::oneshot::Receiver<Result<(), String>>,
    bound: Instant,
) -> Option<Result<(), String>> {
    tokio::time::timeout_at(bound, completion)
        .await
        .ok()
        .map(|result| {
            result.unwrap_or_else(|_| {
                Err("direct command completion owner closed before finalization".into())
            })
        })
}

fn failure_handback_bound(nested_deadline: Option<std::time::Instant>) -> Instant {
    let local = Instant::now() + FAILURE_HANDBACK_GRACE;
    nested_poll_bound(nested_deadline).map_or(local, |bound| bound.min(local))
}

async fn finish_exited_process_result(
    process: Option<&Arc<UnifiedExecProcess>>,
    mut result: Result<ExecCommandToolOutput, UnifiedExecError>,
    duration: Duration,
    hard_deadline: Option<Instant>,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    // A failure already carries the owner's bounded observations. In
    // particular do not wait again after its output has been acknowledged.
    if let Some(process) = process
        && match &result {
            Ok(response) => response.process_exited,
            Err(
                UnifiedExecError::ProcessFailedWithOutput { .. }
                | UnifiedExecError::ToolHistoryPersistence { .. },
            ) => false,
            Err(_) => process.has_exited() && !process.shutdown_is_pending(),
        }
    {
        let completion = match hard_deadline {
            Some(bound) => {
                match tokio::time::timeout_at(bound, process.wait_for_terminal_completion()).await {
                    Ok(completion) => completion,
                    Err(_) => {
                        if let Ok(response) = &mut result
                            && response.process_id.is_some()
                        {
                            let notice = "Process exited; completion bookkeeping is pending. Poll the retained session; do not rerun the command.";
                            // The handler and its caller both finish an exited
                            // process; the second bounded wait must not repeat it.
                            if !response
                                .repair_notice
                                .as_deref()
                                .is_some_and(|receipt| receipt.contains(notice))
                            {
                                response.repair_notice =
                                    Some(match response.repair_notice.take() {
                                        Some(receipt) => format!("{receipt}\n{notice}"),
                                        None => notice.to_string(),
                                    });
                            }
                        }
                        return result;
                    }
                }
            }
            None => {
                match tokio::time::timeout_at(
                    failure_handback_bound(None),
                    process.wait_for_terminal_completion(),
                )
                .await
                {
                    Ok(completion) => completion,
                    Err(_) => return result,
                }
            }
        };
        if let Err(message) = completion {
            let observed = result.ok().map(Box::new);
            return Err(UnifiedExecError::ToolHistoryPersistence {
                message,
                exit_code: process.exit_code().unwrap_or(-1),
                duration,
                event_call_id: observed.as_ref().map(|response| response.event_call_id.clone()),
                output: observed,
            });
        }
    }
    result
}

impl UnifiedExecProcessManager {
    async fn fail_process_with_message(
        &self,
        process_id: u32,
        process: &Arc<UnifiedExecProcess>,
        message: String,
        metadata: FailureOutputContext,
        terminal_tasks: Option<&tokio_util::task::TaskTracker>,
        bound: Instant,
    ) -> UnifiedExecError {
        let started = Instant::now();
        if let Some(tasks) = terminal_tasks {
            let cleanup = {
                // Serialize only the transfer to a tracked owner, never the
                // termination operation. Ordinary output polls do not retry it.
                let _owner_gate = self
                    .pending_cleanup_owners
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if process.termination_was_requested() {
                    None
                } else {
                    process.request_cancellation();
                    let process = Arc::clone(process);
                    let failure = message.clone();
                    let owners = Arc::clone(&self.pending_cleanup_owners);
                    Some(tasks.spawn(async move {
                        let result = process.fail_and_terminate(failure).await;
                        if result.is_ok() {
                            note_background_termination(&owners, process_id, &process);
                        }
                        result
                    }))
                }
            };
            if let Some(cleanup) = cleanup {
                let _ = tokio::time::timeout_at(bound, cleanup).await;
            }
        }
        self.process_failure_output(process_id, process, message, metadata, started, bound)
            .await
    }

    async fn process_failure_output(
        &self,
        process_id: u32,
        process: &Arc<UnifiedExecProcess>,
        mut message: String,
        metadata: FailureOutputContext,
        started: Instant,
        bound: Instant,
    ) -> UnifiedExecError {
        // Observe terminal publication before taking the final snapshot, not
        // after consuming any bytes. A pending publisher retains its own work.
        let terminal_error = if process.has_exited() && !process.shutdown_is_pending() {
            tokio::time::timeout_at(bound, process.wait_for_terminal_completion())
                .await
                .ok()
                .and_then(Result::err)
        } else {
            None
        };
        let process_output = tokio::time::timeout_at(bound, process.snapshot_tool_output(metadata.tty))
            .await
            .ok();
        let raw_output_artifact = tokio::time::timeout_at(bound, process.raw_output_artifact())
            .await
            .ok()
            .flatten()
            .or_else(|| process.try_raw_output_artifact());
        let process_exited = process.has_exited();
        let capabilities = process.session_capabilities(metadata.tty);
        match capabilities.shutdown {
            Some(crate::tools::context::ExecShutdownStatus::ShutdownUncertain) => {
                let cause = process.shutdown_error().unwrap_or_else(|| "outcome is uncertain".into());
                message.push_str(&format!("; process termination was not confirmed: {cause}"));
            }
            Some(crate::tools::context::ExecShutdownStatus::CancellationRequested) => {
                message.push_str("; process termination has not been confirmed; poll the retained session");
            }
            _ => {}
        }
        let mut output = ExecCommandToolOutput {
            stream_chunks: None,
            stream_ranges: None,
            environment_receipt: process.environment_receipt(),
            output_ranges: None,
            process_output,
            error: Some(message.clone()),
            validation: process.validation(),
            event_call_id: metadata.call_id,
            chunk_id: generate_chunk_id(),
            wall_time: started.elapsed(),
            raw_output: format!("\n{message}").into_bytes(),
            truncation_policy: codex_utils_output_truncation::TruncationPolicy::Tokens(8_000),
            max_output_tokens: None,
            quiet_on_success: false,
            process_id: Some(process_id),
            session_capabilities: Some(capabilities),
            exit_code: process_exited.then(|| process.exit_code()).flatten(),
            process_exited,
            search_no_match: false,
            original_token_count: None,
            hook_command: Some(metadata.hook_command),
            tool_cwd: metadata.hook_cwd,
            raw_output_artifact,
            repair_notice: None,
            pending_deferred_completions: Vec::new(),
        };
        let handles = process.output_handles();
        // If either lock contends until the handback bound, report no newly
        // consumed bytes. Neither the worker nor a timed-out poll acknowledges
        // bytes. No suspension follows a successful commit.
        let commit = async {
            let mut store = self.process_store.lock().await;
            let matches = store.processes.get(&process_id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.process, process));
            if !matches {
                output.process_id = None;
                return;
            }
            let mut buffer = handles.output_buffer.lock().await;
            buffer.collect_pending_output();
            let lag_marker = buffer.pending_output()
                .filter(|pending| pending.lagged_chunks() > 0)
                .map(|pending| lagged_output_marker(pending.lagged_chunks()))
                .unwrap_or_default();
            let (mut raw_output, output_ranges) =
                buffer.projected_pending_output(process.output_is_closed(), &lag_marker);
            raw_output.extend_from_slice(&output.raw_output);
            output.raw_output = raw_output;
            output.output_ranges = output_ranges;
            output.stream_ranges = buffer.pending_stream_ranges();
            buffer.acknowledge_pending_output();
            if output.process_exited
                && output.process_output.as_ref().is_some_and(|snapshot| snapshot.output_closed)
                && Self::process_can_retire(process, &buffer)
            {
                store.remove(process_id);
                output.process_id = None;
                store.remember_finished(process_id, &output);
            }
        };
        let _ = tokio::time::timeout_at(bound, commit).await;
        if let Some(error) = terminal_error {
            return UnifiedExecError::ToolHistoryPersistence {
                message: error,
                exit_code: output.exit_code.unwrap_or(-1),
                duration: output.wall_time,
                event_call_id: Some(output.event_call_id.clone()),
                output: Some(Box::new(output)),
            };
        }
        UnifiedExecError::ProcessFailedWithOutput {
            message,
            output: Box::new(output),
        }
    }

    pub(crate) fn effective_environment(
        &self,
        context: &UnifiedExecContext,
    ) -> HashMap<String, String> {
        build_unified_exec_environment(context).0
    }

    pub(crate) async fn reserve_process_id(&self) -> Result<ProcessIdReservation, UnifiedExecError> {
        let process_id = {
            let mut store = self.process_store.lock().await;
            let process_id = store.process_id_history.allocate().ok_or_else(|| {
                UnifiedExecError::process_failed(
                    "unified exec process ID space exhausted; command was not started".to_string(),
                )
            })?;
            store.reserved_process_ids.insert(process_id);
            process_id
        };
        let (transfer_sender, transfer_receiver) = tokio::sync::oneshot::channel();
        let process_store = Arc::clone(&self.process_store);
        tokio::spawn(async move {
            if transfer_receiver.await.is_err() {
                let mut store = process_store.lock().await;
                store.release_reservation(process_id);
            }
        });
        Ok(ProcessIdReservation::new(process_id, transfer_sender))
    }

    pub(crate) async fn exclude_process_ids(&self, ids: impl IntoIterator<Item = u32>) {
        let mut store = self.process_store.lock().await;
        for id in ids {
            store.process_id_history.exclude_restored(id);
        }
    }

    async fn reserve_process_capacity(
        &self,
        reservation: &mut ProcessIdReservation,
    ) -> Result<(), UnifiedExecError> {
        if reservation.capacity_reserved {
            return Ok(());
        }
        let pruned_entry = {
            let mut store = self.process_store.lock().await;
            let pruned_entry = Self::prune_processes_if_needed(&mut store);
            if store.occupied_slots() >= MAX_UNIFIED_EXEC_PROCESSES {
                return Err(UnifiedExecError::process_failed(format!(
                    "unified exec process limit ({MAX_UNIFIED_EXEC_PROCESSES}) reached; all slots are active, reserved, or awaiting output recovery; command was not started"
                )));
            }
            // Hold admission across approvals, launch, and the initial response.
            // The existing reservation owns cancellation cleanup until transfer.
            store
                .reserved_process_slots
                .insert(reservation.process_id());
            reservation.capacity_reserved = true;
            pruned_entry
        };
        // Never await a process/output owner while holding the global store.
        if let Some(entry) = pruned_entry
            && let Some(session) = entry.session.upgrade()
        {
            session
                .services
                .command_execution
                .finish_running_process_with_execution_id(
                    entry.process_id,
                    entry.command_execution_id,
                    &entry.parent_tool_execution_id,
                    entry.process.exit_code(),
                )
                .await;
        }
        Ok(())
    }



    pub(crate) async fn release_process_id(&self, process_id: u32) {
        let removed = {
            let mut store = self.process_store.lock().await;
            store.remove(process_id)
        };
        // The entry may own the last process handle; drop it outside the store lock.
        drop(removed);
    }

    pub(super) async fn release_failed_completion(
        &self,
        process_id: u32,
        process: &Arc<UnifiedExecProcess>,
    ) {
        let removed = {
            let mut store = self.process_store.lock().await;
            let matches = store
                .processes
                .get(&process_id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.process, process));
            // Persistence failure cannot retire unconfirmed cancellation custody.
            (matches && process.has_exited() && !process.shutdown_is_pending())
                .then(|| store.remove(process_id))
                .flatten()
        };
        drop(removed);
    }

    pub(crate) async fn exec_command(
        &self,
        mut request: ExecCommandRequest,
        mut process_id_reservation: ProcessIdReservation,
        context: &UnifiedExecContext,
        cancellation_token: &CancellationToken,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        debug_assert_eq!(request.process_id, process_id_reservation.process_id());
        let mut registration = PendingProcessRegistration::new(
            Arc::clone(&self.process_store),
            context,
            request.attempt_key.clone(),
            request.process_id,
        );
        // Transfer the existing reservation to provisional custody before the
        // first await. Cleanup, commit, or a confirmed exit now releases it.
        process_id_reservation.transfer_to_store();
        let request_started_at = Instant::now();
        let mut cancelled = false;
        // Finish an accepted event and its legacy dispatch before cancellation
        // takes ownership of the command's terminal transition.
        let event_delivery = tokio::sync::Mutex::new(());
        let mut result = tokio::select! {
            biased;
            _ = async {
                cancellation_token.cancelled().await;
                let _event_delivery = event_delivery.lock().await;
            } => {
                cancelled = true;
                Err(UnifiedExecError::process_failed(
                    "unified exec cancelled".to_string(),
                ))
            }
            result = self.exec_command_inner(
                &mut request,
                &mut process_id_reservation,
                context,
                &mut registration,
                &event_delivery,
            ) => result,
        };
        let failure_bound = failure_handback_bound(context.source.nested_deadline());
        if let Err(error) = &result
            && let Some(startup) = registration.pending_startup_completion.take()
        {
            // Begin was accepted, but registration never assigned an exit
            // watcher. Hand its unique End and cleanup to the session tracker.
            let completion = start_direct_completion(
                &mut registration,
                context,
                &request,
                startup.cwd,
                startup.transcript,
                String::new(),
                false,
                Some(error.to_string()),
                startup.started_at.elapsed(),
                false,
            );
            if let Some(Err(persistence_error)) = wait_for_direct_completion(completion, failure_bound).await {
                result = Err(UnifiedExecError::process_failed(format!(
                    "{error}; failed to persist command completion: {persistence_error}"
                )));
            }
        } else if cancelled && registration.committed {
            if let Some(process) = registration.primary_process.as_ref() {
                result = Err(self
                    .fail_process_with_message(
                        request.process_id,
                        process,
                        "unified exec cancelled".into(),
                        FailureOutputContext::for_exec(context, &request),
                        Some(&context.session.terminal_tasks),
                        failure_bound,
                    )
                    .await);
                if process.has_exited() {
                    mark_exec_process_exited();
                }
            }
        } else if !registration.committed {
            // No Begin obligation remains. If provisional shutdown or its
            // bookkeeping cannot finish soon, Drop hands it to its tracked owner.
            let cleanup = tokio::time::timeout_at(failure_bound, registration.cleanup()).await;
            let cleanup_error = match cleanup {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(format!("unified exec startup cleanup failed: {error}")),
                Err(_) => Some("unified exec startup cleanup remains pending; do not rerun automatically".into()),
            };
            if let Some(cleanup_error) = cleanup_error {
                match &mut result {
                    Err(UnifiedExecError::ToolHistoryPersistence { message, .. }) => {
                        message.push_str(&format!("; {cleanup_error}"));
                    }
                    _ => {
                        let message = result
                            .as_ref()
                            .err()
                            .map_or_else(|| cleanup_error.clone(), |error| format!("{error}; {cleanup_error}"));
                        result = Err(UnifiedExecError::process_failed(message));
                    }
                }
            }
            if cancelled
                && registration.primary_process.as_ref().is_some_and(|process| process.has_exited())
            {
                mark_exec_process_exited();
            }
        }
        let result = finish_exited_process_result(
            registration.primary_process.as_ref(),
            result,
            Instant::now().saturating_duration_since(request_started_at),
            registration
                .observation_deadline
                .or_else(|| nested_poll_bound(context.source.nested_deadline())),
        )
        .await;
        tracing::info!(event.name = "codex.exec.handler_completed",
            operation = "exec_command", call_id = %context.call_id,
            handler_elapsed_ms = u64::try_from(request_started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            success = result.is_ok(),
            "handler elapsed includes launch, observation and result finalization");
        result
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Keep the request-local cancellation gate held until command lifecycle event delivery finishes"
    )]
    async fn exec_command_inner(
        &self,
        request: &mut ExecCommandRequest,
        process_id_reservation: &mut ProcessIdReservation,
        context: &UnifiedExecContext,
        registration: &mut PendingProcessRegistration,
        event_delivery: &tokio::sync::Mutex<()>,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let cwd = request.cwd.clone();
        // Hooks run on the host; only a local command cwd can replace the host cwd.
        let hook_cwd = (!request.turn_environment.environment.is_remote()).then(|| cwd.clone());
        let mut tool_history_error = None;
        self.reserve_process_capacity(process_id_reservation)
            .await?;
        let executor_started_at = Instant::now();
        let tool_execution_timing_guard = context.turn.turn_timing_state.begin_tool_execution();
        let launch = self
            .open_session_with_sandbox(request, cwd.clone(), context, registration.pending_spawns())
            .await;

        let process = launch?;
        process.set_stdin_enabled(request.tty || request.pipe_stdin);
        process.set_validation(request.validation.clone());
        let _ = process.environment.set(request.turn_environment.clone());
        let mut stall_handoff = Some(Box::pin(process.interaction_requested().notified_owned()));
        if let Some(notified) = stall_handoff.as_mut() {
            notified.as_mut().enable();
        }
        process.start_stall_watchdog(request.stall_timeout_ms);
        let validation_process = request.validation_launch || request.validation.is_some();
        registration.attach_process(Arc::clone(&process));
        let executor_was_ready = self.mark_executor_ready(&request.turn_environment.environment_id);

        let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
        let event_ctx = ToolEventCtx::new(
            context.session.as_ref(),
            context.turn.as_ref(),
            &context.call_id,
            context.tracker.as_ref(),
        )
        .with_call_source(&context.source);
        let emitter = ToolEmitter::unified_exec(
            &request.command_for_safety,
            cwd.clone(),
            ExecCommandSource::UnifiedExecStartup,
            Some(request.process_id.to_string()),
        )
        .with_captured_environment(&request.turn_environment);
        let event_delivery_guard = event_delivery.lock().await;
        let item = emitter.unified_exec_startup_item(event_ctx);
        let (event, append) = context
            .session
            .prepare_unified_exec_command_start(&context.turn, item);
        // Install custody before the first await. Dropping this call can no
        // longer drop the exact ordered receipt, even before it is observed.
        let startup = registration.pending_startup_completion.insert(PendingStartupCompletion {
            begin: append.map_or(StartupBegin::LiveOnly, StartupBegin::Appending),
            turn: Arc::clone(&context.turn),
            call_id: context.call_id.clone(),
            command: request.command_for_safety.clone(),
            cwd: cwd.clone(),
            transcript: Arc::clone(&transcript),
            started_at: Instant::now(),
            tty: request.tty,
            source: context.source.clone(),
            tracker: context.tracker.clone(),
            timing: active_tool_dispatch_timing(),
        });
        startup.observe_begin_append().await;
        if let StartupBegin::Unconfirmed { observed, message } = &startup.begin {
            let observed = *observed;
            let message = message.clone();
            let append = {
                let mut owners = registration
                    .cleanup_owners
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                registration.register_startup_append(&mut owners, &process)
            };
            append.settle(Some((observed, message.clone())));
            registration.pending_startup_completion = None;
            return Err(UnifiedExecError::process_failed(message));
        }
        context
            .session
            .dispatch_unified_exec_command_start(&context.turn, event, || {
                startup.begin = StartupBegin::Accepted;
            })
            .await;

        let start = Instant::now();
        startup.started_at = start;
        drop(event_delivery_guard);
        start_streaming_output(&process, context, Arc::clone(&transcript))?;
        // Persist live sessions before the initial yield wait so handler cancellation cannot
        // orphan the process. Mutating sessions are explicitly terminated when their owning turn
        // reaches a terminal state; non-mutating sessions remain resumable across turns.
        let process_started_alive = (!process.has_exited() && process.exit_code().is_none())
            || !process.output_is_closed()
            || context.source.nested_deadline().is_some();
        let _initial_exec_command_guard = if process_started_alive {
            let initial_exec_command_active = Arc::new(AtomicBool::new(true));
            registration.set_initial_exec_command_active(Arc::clone(&initial_exec_command_active));
            let initial_exec_command_guard = InitialExecCommandGuard {
                active: Arc::clone(&initial_exec_command_active),
            };
            let store_result = self
                .store_process(
                    Arc::clone(&process),
                    context,
                    &request.command_for_safety,
                    request.hook_command.clone(),
                    hook_cwd.clone(),
                    cwd.clone(),
                    start,
                    request.process_id,
                    process_id_reservation,
                    request.tty,
                    request.attempt_key.clone(),
                    request.raw_output_artifact.clone(),
                    Arc::clone(&transcript),
                    Arc::clone(&initial_exec_command_active),
                    registration,
                    !request.tty && request.validation_launch,
                    validation_process.then_some(executor_started_at),
                )
                .await;
            store_result?;
            Some(initial_exec_command_guard)
        } else {
            None
        };

        let yield_time_ms = clamp_initial_yield_time(request.yield_time_ms);
        let cold_start_allowance = cold_start_yield_allowance(yield_time_ms, executor_was_ready);
        // Registration above already completed, so an initial launch that runs
        // out of nested budget here still returns a registered process id.
        let poll_bound = nested_poll_bound(context.source.nested_deadline());
        // For the initial exec_command call, we both stream output to events
        // (via start_streaming_output above) and collect a snapshot here for
        // the tool response body.
        let OutputHandles {
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
            ..
        } = process.output_handles();
        let deadline = match poll_bound {
            Some(bound) => (start + Duration::from_millis(yield_time_ms)).min(bound),
            None => start + Duration::from_millis(yield_time_ms),
        };
        // Finalizers keep the full cold-start bound even when the wait below
        // ends at the requested time.
        registration.observation_deadline = Some(match poll_bound {
            Some(bound) => (deadline + cold_start_allowance).min(bound),
            None => deadline + cold_start_allowance,
        });
        // A burst from a noninteractive command is progress, not completion.
        // Keep its existing observation window attached across quiet periods;
        // interactive terminals still yield promptly when a prompt is ready.
        let quiet_period = request.tty.then_some(INITIAL_OUTPUT_QUIET_PERIOD);
        let collected = Self::collect_output_until_deadline_with_quiet_yield(
            &output_buffer,
            &output_notify,
            &output_closed,
            &output_closed_notify,
            &cancellation_token,
            Some(context.session.subscribe_elicitation_pause_state()),
            deadline,
            poll_bound,
            quiet_period,
            cold_start_allowance,
            &mut stall_handoff,
        )
        .await;
        let deadline = collected.observation_deadline;
        registration.observation_deadline = Some(deadline);
        let output_ranges = collected.ranges;
        let stream_ranges = collected.stream_ranges;
        let collected = collected.bytes;
        // Root exit is not output closure. All post-observation finalizers,
        // including the outer caller, share the collector's pause-adjusted
        // observation bound, still capped by any enclosing hard deadline.
        let poll_bound = Some(deadline);
        // Observer shutdown wakes the collector but is not an exit receipt.
        // The independent watcher records a later confirmed termination.
        if process.has_exited() {
            mark_exec_process_exited();
        }
        if !process_started_alive && validation_process {
            context
                .turn
                .turn_timing_state
                .record_executed_validation_duration(executor_started_at.elapsed());
        }
        drop(tool_execution_timing_guard);
        let wall_time = Instant::now().saturating_duration_since(start);

        let text = String::from_utf8_lossy(&collected).into_owned();
        let chunk_id = generate_chunk_id();
        let failure = process.failure_message();
        if process_started_alive && let Some(message) = failure.as_ref() {
            return Err(self
                .fail_process_with_message(
                    request.process_id,
                    &process,
                    message.clone(),
                    FailureOutputContext::for_exec(context, request),
                    Some(&context.session.terminal_tasks),
                    failure_handback_bound(context.source.nested_deadline()),
                )
                .await);
        }
        let mut completion_pending = false;
        let process_id = request.process_id;
        let (response_process_id, exit_code, process_exited) = if process_started_alive {
            match self.refresh_process_state(process_id, &process).await {
                ProcessStatus::Running {
                    exit_code,
                    process_id,
                    ..
                } => (Some(process_id), exit_code, false),
                ProcessStatus::OutputPending {
                    exit_code,
                    process_id,
                    ..
                } => (Some(process_id), exit_code, true),
                ProcessStatus::Exited { exit_code, .. } => {
                    process.check_for_sandbox_denial_with_text(&text).await?;
                    (Some(process_id), exit_code, true)
                }
                ProcessStatus::Unknown => {
                    return Err(UnifiedExecError::UnknownProcessId { process_id });
                }
            }
        } else {
            let exit_code = process.exit_code();
            let classified_search = request.attempt_key.is_search_no_match(exit_code)
                || (!validation_process
                    && crate::tools::shell_output_summary::ends_with_native_search(
                        &request.hook_command,
                    ));
            let startup = registration
                .pending_startup_completion
                .take()
                .expect("a direct process still owns its accepted Begin");
            let completion = start_direct_completion(
                registration,
                context,
                request,
                startup.cwd,
                startup.transcript,
                text.clone(),
                failure.is_none() && classified_search,
                failure.clone(),
                wall_time,
                true,
            );
            match wait_for_direct_completion(completion, deadline).await {
                Some(result) => tool_history_error = result.err(),
                None => completion_pending = true,
            }

            if tool_history_error.is_none() && failure.is_none() {
                process.check_for_sandbox_denial_with_text(&text).await?;
            }
            (None, exit_code, true)
        };

        let original_token_count = approx_token_count(&text);
        let response = ExecCommandToolOutput {
            stream_chunks: if !process_started_alive {
                tokio::time::timeout_at(deadline, process.stream_chunks(stream_ranges.as_ref(), request.tty))
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            },
            process_output: if !process_started_alive {
                tokio::time::timeout_at(deadline, process.snapshot_tool_output(request.tty))
                    .await
                    .ok()
            } else {
                None
            },
            error: failure.clone(),
            validation: process.validation(),
            environment_receipt: process.environment_receipt(),
            event_call_id: context.call_id.clone(),
            chunk_id,
            wall_time: start.elapsed(),
            raw_output: collected,
            output_ranges,
            stream_ranges,
            truncation_policy: context.turn.model_info.truncation_policy.into(),
            max_output_tokens: request.max_output_tokens,
            quiet_on_success: false,
            process_id: response_process_id,
            session_capabilities: response_process_id
                .map(|_| process.session_capabilities(request.tty)),
            exit_code,
            process_exited,
            search_no_match: request.attempt_key.is_search_no_match(exit_code)
                || (exit_code == Some(1)
                    && !validation_process
                    && crate::tools::shell_output_summary::ends_with_native_search(
                        &request.hook_command,
                    )),
            original_token_count: Some(original_token_count),
            hook_command: Some(request.hook_command.clone()),
            tool_cwd: hook_cwd,
            raw_output_artifact: match poll_bound {
                Some(bound) => tokio::time::timeout_at(bound, process.raw_output_artifact())
                    .await
                    .ok()
                    .flatten()
                    .or_else(|| process.try_raw_output_artifact()),
                None => process.raw_output_artifact().await,
            },
            repair_notice: completion_pending.then(|| {
                "Process exited; command completion is still being recorded. Do not rerun the command.".into()
            }),
            pending_deferred_completions: Vec::new(),
        };

        if process_started_alive
            && let Some(finalized_artifact) = response.raw_output_artifact.clone()
        {
            let update = context
                .session
                .services
                .command_execution
                .update_running_artifact(process_id, finalized_artifact);
            match poll_bound {
                Some(bound) => {
                    let _ = tokio::time::timeout_at(bound, update).await;
                }
                None => update.await,
            }
        }

        if let Some(error) = tool_history_error {
            return Err(UnifiedExecError::ToolHistoryPersistence {
                message: error,
                exit_code: response.exit_code.unwrap_or(-1),
                duration: response.wall_time,
                event_call_id: Some(response.event_call_id.clone()),
                output: Some(Box::new(response)),
            });
        }
        // Direct output has no store consumer or replay cursor. Return the
        // captured response even when its tracked End has not persisted yet.
        if !process_started_alive {
            return match failure {
                Some(message) => Err(UnifiedExecError::ProcessFailedWithOutput {
                    message,
                    output: Box::new(response),
                }),
                None => Ok(response),
            };
        }
        let mut response =
            finish_exited_process_result(Some(&process), Ok(response), start.elapsed(), poll_bound)
                .await?;
        if !response.process_exited || process.terminal_completion_is_ready() {
            let commit =
                self.acknowledge_output(&process, &output_buffer, request.tty, &mut response);
            let committed = match poll_bound {
                Some(bound) => {
                    tokio::time::timeout_at(bound, commit).await.is_ok()
                }
                None => { commit.await; true }
            };
            if !committed {
                response.withhold_unacknowledged_output();
            }
        } else {
            response.withhold_unacknowledged_output();
        }
        Ok(response)
    }

    pub(crate) async fn write_stdin(
        &self,
        request: WriteStdinRequest<'_>,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        self.write_stdin_with_observation_mode(request, WriteStdinObservation::Poll, None)
            .await
    }

    /// Report whether the backend was polled, independently of the child result.
    pub(crate) async fn write_stdin_interaction(
        &self,
        request: WriteStdinRequest<'_>,
        cancellation: &CancellationToken,
    ) -> (Result<ExecCommandToolOutput, UnifiedExecError>, bool) {
        let mut mutation = StdinMutation {
            cancellation,
            started: false,
        };
        let result = self
            .write_stdin_with_observation_mode(
                request,
                WriteStdinObservation::Poll,
                Some(&mut mutation),
            )
            .await;
        (result, mutation.started)
    }

    pub(crate) async fn write_stdin_until_output(
        &self,
        request: WriteStdinRequest<'_>,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        self.write_stdin_with_observation_mode(request, WriteStdinObservation::UntilOutput, None)
            .await
    }

    async fn write_stdin_with_observation_mode(
        &self,
        request: WriteStdinRequest<'_>,
        observation: WriteStdinObservation,
        mutation: Option<&mut StdinMutation<'_>>,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let handler_started_at = Instant::now();
        let process_id = request.process_id;
        let poll_bound = nested_poll_bound(request.nested_deadline);
        let process = {
            let store = self.process_store.lock().await;
            if observation != WriteStdinObservation::BackgroundCompletion
                && let Some(response) = Self::finished_stdin_response(&store, &request)
            {
                return response;
            }
            store
                .processes
                .get(&request.process_id)
                .map(|entry| Arc::clone(&entry.process))
                .ok_or(UnifiedExecError::UnknownProcessId {
                    process_id: request.process_id,
                })?
        };
        let started_at = Instant::now();
        let mut hard_deadline = poll_bound;
        let result = self
            .write_stdin_inner(request, &process, observation, mutation, &mut hard_deadline)
            .await;
        let result = if observation == WriteStdinObservation::BackgroundCompletion {
            // The exit watcher was already settled when this poll was admitted.
            // The inner poll finalized successful output before acknowledging it;
            // output-bearing errors are delivered as-is by the background collector.
            // Do not suspend between consuming output and handing it to that owner.
            result
        } else {
            finish_exited_process_result(
                Some(&process),
                result,
                Instant::now().saturating_duration_since(started_at),
                hard_deadline,
            )
            .await
        };
        tracing::info!(event.name = "codex.exec.handler_completed",
            operation = "write_stdin", process_id,
            handler_elapsed_ms = u64::try_from(handler_started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            success = result.is_ok(),
            "handler elapsed includes interaction admission and result finalization");
        result
    }

    async fn write_stdin_inner(
        &self,
        request: WriteStdinRequest<'_>,
        locked_process: &Arc<UnifiedExecProcess>,
        observation: WriteStdinObservation,
        mutation: Option<&mut StdinMutation<'_>>,
        observation_deadline: &mut Option<Instant>,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let process_id = request.process_id;

        // Capture stable controls before spending the interaction-queue budget.
        // Timeout handback must not reacquire store or artifact readiness locks.
        let prepared_handles = match self
            .prepare_process_handles(process_id, locked_process)
            .await
        {
            Ok(handles) => handles,
            Err(error) => {
                if observation == WriteStdinObservation::BackgroundCompletion {
                    return Err(error);
                }
                let store = self.process_store.lock().await;
                return Self::finished_stdin_response(&store, &request).unwrap_or(Err(error));
            }
        };

        // Register polls before joining the FIFO lock queue. A writer first
        // joins that queue, then wakes registered polls; no notification can be
        // lost between acquiring the lock and starting output collection.
        let interaction_requested = locked_process.interaction_requested();
        let input_operation = !request.input.is_empty() || request.close_stdin;
        let mut handoff = (!input_operation).then(|| {
            let mut notified = Box::pin(Arc::clone(&interaction_requested).notified_owned());
            notified.as_mut().enable();
            notified
        });
        let queue_started_at = Instant::now();
        // Queueing behind another interaction is charged against the nested
        // budget like every other pre-observation stage, so a call that waits
        // out its budget here still reports a live process instead of being
        // cancelled at the wrapper's hard deadline.
        let poll_bound = if input_operation {
            *observation_deadline
        } else {
            nested_poll_bound(request.nested_deadline)
        };
        let cancellation = mutation.as_ref().map(|mutation| mutation.cancellation);
        let lock = locked_process.interaction_lock().lock_owned();
        tokio::pin!(lock);
        let guard = async {
            if !input_operation {
                lock.await
            } else {
                match futures::poll!(&mut lock) {
                    std::task::Poll::Ready(guard) => guard,
                    std::task::Poll::Pending => {
                        interaction_requested.notify_waiters();
                        lock.await
                    }
                }
            }
        };
        let guard = if input_operation {
            poll_stdin_before(poll_bound, cancellation, guard).await
        } else {
            match poll_bound {
                Some(bound) => tokio::time::timeout_at(bound, guard).await.ok(),
                None => Some(guard.await),
            }
        };
        let not_admitted = || {
            Self::unadmitted_interaction_result(
                &prepared_handles,
                &request,
                queue_started_at.elapsed(),
            )
        };
        let Some(_interaction_guard) = guard else {
            tracing::debug!(
                process_id,
                interaction_queue_wait_ms = queue_started_at.elapsed().as_millis(),
                "unified exec interaction not admitted within the owner budget"
            );
            return Ok(not_admitted());
        };
        tracing::debug!(
            process_id,
            interaction_queue_wait_ms = queue_started_at.elapsed().as_millis(),
            "unified exec interaction lock acquired"
        );

        // A preceding poll may have retired this process while we queued.
        let store = self.process_store.lock();
        let store = if input_operation {
            let Some(store) = poll_stdin_before(poll_bound, cancellation, store).await else {
                return Ok(not_admitted());
            };
            store
        } else {
            store.await
        };
        if observation == WriteStdinObservation::BackgroundCompletion {
            if !store.processes.get(&process_id).is_some_and(|entry| {
                Arc::ptr_eq(&entry.process, locked_process)
                    && Self::background_completion_ready(entry)
            }) {
                return Err(UnifiedExecError::UnknownProcessId { process_id });
            }
        } else if let Some(response) = Self::finished_stdin_response(&store, &request) {
            return response;
        }
        drop(store);
        if request.acknowledge_only && prepared_handles.tty {
            return Err(UnifiedExecError::process_failed(
                "acknowledge_only requires piped stdin, not a terminal; input was not delivered".into(),
            ));
        }
        if request.close_stdin && prepared_handles.tty {
            return Err(UnifiedExecError::CloseStdin {
                message: "terminal input cannot be half-closed; use pipe_stdin=true with tty=false, not Ctrl-D".into(),
            });
        }
        if !request.input.is_empty() && locked_process.has_exited() {
            return Err(UnifiedExecError::process_failed(format!(
                "process {process_id} has exited; input was not delivered",
            )));
        }

        let mut status_after_write = None;
        let mut input_error = None;
        let yield_time_ms = {
            // Empty polls use configurable background timeout bounds. Non-empty
            // writes keep a fixed max cap so interactive stdin remains responsive.
            // Only undelivered output justifies a sub-five-second empty poll.
            // The interaction guard serializes this check with acknowledgement.
            let new_output = if input_operation {
                false
            } else {
                let output = prepared_handles.output_buffer.lock().await;
                output.has_uncollected_output()
                    || output
                        .pending_output()
                        .is_some_and(HeadTailBuffer::has_uncollected_output)
            };
            let minimum = if !input_operation && !new_output {
                crate::unified_exec::MIN_EMPTY_YIELD_TIME_MS
            } else {
                MIN_YIELD_TIME_MS
            };
            let time_ms = request.yield_time_ms.max(minimum);
            if !input_operation {
                time_ms.min(self.max_write_stdin_yield_time_ms)
            } else {
                time_ms.min(MAX_YIELD_TIME_MS)
            }
        };
        // The yield timeout bounds the write and process-reaction window.
        // Failure cleanup confirms termination under its separate deadline.
        let start = Instant::now();
        // Whatever the caller asked for, observation ends inside the nested
        // budget that queueing has already drawn down. The minimum yield
        // durations above bound the request, never the enclosing deadline.
        let deadline = match poll_bound {
            Some(bound) => (start + Duration::from_millis(yield_time_ms)).min(bound),
            None => start + Duration::from_millis(yield_time_ms),
        };
        *observation_deadline = Some(deadline);
        let effective_timeout_ms =
            u64::try_from(deadline.saturating_duration_since(start).as_millis())
                .unwrap_or(u64::MAX);
        let mut wait = WriteStdinWait(ToolLifecycleTimerWait {
            wait_kind: "write_stdin_yield".to_string(),
            requested_timeout_ms: Some(request.yield_time_ms),
            effective_timeout_ms: Some(effective_timeout_ms),
            deadline_at_ms: active_tool_dispatch_timing()
                .and_then(|timing| timing.deadline_after_ms(effective_timeout_ms)),
            wake_reason: ToolLifecycleWakeReason::Cancelled,
            sequence: 0,
        });

        if input_operation {
            let process = &prepared_handles.process;
            let interrupt = !prepared_handles.tty && request.input == INTERRUPT;
            // The token prevents first dispatch; once polled, use the existing
            // bounded delivery/cleanup path rather than abandoning remote effects.
            let mut started = false;
            let write = poll_stdin_before(Some(deadline), None, async {
                if cancellation.is_some_and(CancellationToken::is_cancelled) {
                    return None;
                }
                started = true;
                Some(if request.close_stdin {
                    process.close_stdin().await
                } else if interrupt {
                    process.interrupt().await
                } else {
                    process.write(request.input.as_bytes()).await
                })
            })
            .await
            .flatten();
            if !started {
                return Ok(not_admitted());
            }
            if let Some(mutation) = mutation {
                mutation.started = true;
            }
            if request.close_stdin {
                write.ok_or_else(|| UnifiedExecError::CloseStdin {
                    message: "close requested but pipe drain exceeded the yield deadline; the process remains available".into(),
                })??;
            } else if interrupt {
                write.ok_or_else(|| UnifiedExecError::process_failed(
                    "interrupt delivery was not confirmed before the yield deadline; the signal was not retried and the session remains available".to_string(),
                ))??;
            } else {
                let write = match write {
                    Some(write) => write,
                    // The backend guarantees the abandoned write queued nothing, so the
                    // process keeps running. Input carrying Ctrl-C still ends it below,
                    // because the interrupt it asked for was not delivered.
                    None
                        if process.cancelled_write_is_unsent()
                            && !request.input.contains(INTERRUPT) =>
                    {
                        return Err(UnifiedExecError::process_failed(
                            "stdin was not written before the yield deadline; nothing was queued, so retrying is safe and the session remains available".to_string(),
                        ));
                    }
                    None => {
                        // Delivery may have reached the executor before its acknowledgement.
                        // Do not replay uncertain bytes. Confirm termination with its separate
                        // cleanup bound, retaining custody if confirmation fails.
                        return Err(self
                            .fail_process_with_message(
                                process_id,
                                process,
                                "stdin delivery was not confirmed before the yield deadline; do not resend automatically".to_string(),
                                FailureOutputContext::for_stdin(&prepared_handles),
                                prepared_handles.terminal_tasks.as_ref(),
                                failure_handback_bound(request.nested_deadline),
                            )
                            .await);
                    }
                };
                match write {
                    Ok(()) => {}
                    Err(err) => {
                        let bound = failure_handback_bound(request.nested_deadline);
                        let status = tokio::time::timeout_at(bound, self.refresh_process_state(process_id, process)).await.ok();
                        if matches!(status, Some(ProcessStatus::Exited { .. })) {
                            input_error = Some(match err {
                                UnifiedExecError::StdinClosed => "Input was not delivered because stdin closed. The process exit does not acknowledge this interaction.".to_string(),
                                error => format!("Input delivery was not acknowledged: {error}. Delivery is uncertain; do not resend automatically. The process exit is independent."),
                            });
                            status_after_write = status;
                        } else if let UnifiedExecError::ProcessFailed { message } = err {
                            return Err(self
                                .fail_process_with_message(
                                    process_id,
                                    process,
                                    format!("{message}; input delivery may be uncertain; do not resend automatically"),
                                    FailureOutputContext::for_stdin(&prepared_handles),
                                    prepared_handles.terminal_tasks.as_ref(),
                                    bound,
                                )
                                .await);
                        } else {
                            return Err(err);
                        }
                    }
                }
            }
        }

        let failure_metadata = FailureOutputContext::for_stdin(&prepared_handles);
        let PreparedProcessHandles {
            process,
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
            pause_state,
            call_id,
            hook_command,
            hook_cwd,
            process_id,
            search_exit_one_is_no_match,
            tty,
            validation_launch,
            terminal_tasks,
            ..
        } = prepared_handles;

        // The write still used the full bounded delivery deadline. Do not
        // wait for a response after delivery when the caller will send EOF.
        let (deadline, collection_bound) = if request.acknowledge_only {
            let now = Instant::now();
            (now, Some(now))
        } else {
            (deadline, poll_bound)
        };
        let collected = Self::collect_output_until_deadline_with_quiet_yield(
            &output_buffer,
            &output_notify,
            &output_closed,
            &output_closed_notify,
            &cancellation_token,
            pause_state,
            deadline,
            collection_bound,
            // Validation polls continue through progress bursts even in
            // until-output mode, bounded by exit, handoff, or the deadline.
            (!input_operation
                && (observation == WriteStdinObservation::UntilOutput || tty)
                && !validation_launch
                && process.validation().is_none())
            .then_some(INITIAL_OUTPUT_QUIET_PERIOD),
            Duration::ZERO,
            &mut handoff,
        )
        .await;
        wait.0.wake_reason = collected.wake_reason;
        let deadline = collected.observation_deadline;
        *observation_deadline = Some(deadline);
        let output_ranges = collected.ranges;
        let stream_ranges = collected.stream_ranges;
        let collected = collected.bytes;
        let wall_time = Instant::now().saturating_duration_since(start);
        let poll_bound = Some(deadline);

        let chunk_id = generate_chunk_id();
        if let Some(message) = process.failure_message() {
            return Err(self
                .fail_process_with_message(
                    process_id,
                    &process,
                    message,
                    failure_metadata,
                    terminal_tasks.as_ref(),
                    failure_handback_bound(request.nested_deadline),
                )
                .await);
        }

        // After polling, refresh_process_state tells us whether the PTY is
        // still alive or has exited and been removed from the store; we thread
        // that through so the handler can tag or suppress TerminalInteraction
        // with an appropriate process_id and exit_code.
        let status = if let Some(status) = status_after_write {
            status
        } else {
            self.refresh_process_state(process_id, &process).await
        };
        let (process_id, exit_code, process_exited, event_call_id) = match status {
            ProcessStatus::Running {
                exit_code,
                call_id,
                process_id,
            } => (Some(process_id), exit_code, false, call_id),
            ProcessStatus::OutputPending {
                exit_code,
                call_id,
                process_id,
            } => (Some(process_id), exit_code, true, call_id),
            ProcessStatus::Exited { exit_code, entry } => {
                let call_id = entry.call_id.clone();
                (Some(request.process_id), exit_code, true, call_id)
            }
            ProcessStatus::Unknown => {
                if process.has_exited() {
                    (None, process.exit_code(), true, call_id)
                } else {
                    return Err(UnifiedExecError::UnknownProcessId {
                        process_id: request.process_id,
                    });
                }
            }
        };
        let text = String::from_utf8_lossy(&collected).into_owned();
        let original_token_count = approx_token_count(&text);

        let response = ExecCommandToolOutput {
            stream_chunks: None,
            process_output: if process_exited && process_id.is_none() {
                Some(process.snapshot_tool_output(tty).await)
            } else {
                None
            },
            error: input_error,
            environment_receipt: process.environment_receipt(),
            validation: process.validation(),
            event_call_id,
            chunk_id,
            wall_time,
            raw_output: collected,
            output_ranges,
            stream_ranges,
            truncation_policy: request.truncation_policy,
            max_output_tokens: request.max_output_tokens,
            quiet_on_success: false,
            process_id,
            session_capabilities: process_id.map(|_| process.session_capabilities(tty)),
            exit_code,
            process_exited,
            search_no_match: search_exit_one_is_no_match && exit_code == Some(1),
            original_token_count: Some(original_token_count),
            hook_command: Some(hook_command),
            tool_cwd: hook_cwd,
            raw_output_artifact: match poll_bound {
                Some(bound) => tokio::time::timeout_at(bound, process.raw_output_artifact())
                    .await
                    .ok()
                    .flatten(),
                None => process.raw_output_artifact().await,
            },
            repair_notice: None,
            pending_deferred_completions: Vec::new(),
        };

        let mut response =
            finish_exited_process_result(Some(&process), Ok(response), start.elapsed(), poll_bound)
                .await?;
        if !response.process_exited || process.terminal_completion_is_ready() {
            let commit = self.acknowledge_output(&process, &output_buffer, tty, &mut response);
            let committed = match poll_bound {
                Some(bound) => {
                    tokio::time::timeout_at(bound, commit).await.is_ok()
                }
                None => { commit.await; true }
            };
            if !committed {
                response.withhold_unacknowledged_output();
            }
        } else {
            response.withhold_unacknowledged_output();
        }
        Ok(response)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Acquire both owners before acknowledging so cancellation cannot drop a committed result"
    )]
    async fn acknowledge_output(
        &self,
        process: &Arc<UnifiedExecProcess>,
        output_buffer: &OutputBuffer,
        tty: bool,
        response: &mut ExecCommandToolOutput,
    ) {
        // Capture immutable terminal streams before taking either commit lock.
        // A response prepared while output was pending has no snapshot yet.
        let terminal_output = if response.process_exited && process.output_is_closed() {
            Some(process.snapshot_tool_output(tty).await)
        } else {
            None
        };
        let stream_chunks = process.stream_chunks(response.stream_ranges.as_ref(), tty).await;
        // Acquire both owners before acknowledging. No await follows the commit,
        // so cancellation cannot consume evidence and then lose the result.
        let mut store = self.process_store.lock().await;
        let mut buffer = output_buffer.lock().await;
        response.stream_chunks = stream_chunks;
        buffer.acknowledge_pending_output();
        if response.process_exited
            && terminal_output.is_some()
            && Self::process_can_retire(process, &buffer)
            && let Some(id) = response.process_id
            && store
                .processes
                .get(&id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.process, process))
        {
            store.remove(id);
            response.process_id = None;
            response.session_capabilities = process.shutdown_status().map(|_| process.session_capabilities(tty));
            response.process_output = terminal_output;
            store.remember_finished(id, response);
        }
    }

    /// Consume only an already-complete exact incarnation. Every fallible
    /// readiness check precedes the output acknowledgement/finished-result commit.
    pub(crate) fn try_finish_for_packet(
        &self,
        process_id: u32,
        incarnation: Uuid,
        truncation_policy: codex_utils_output_truncation::TruncationPolicy,
        max_output_tokens: Option<usize>,
    ) -> Option<ExecCommandToolOutput> {
        let entry = {
            let store = self.process_store.try_lock().ok()?;
            if let Some(entry) = store.processes.get(&process_id) {
                if entry.process.incarnation() != incarnation
                    || entry.initial_exec_command_active.load(Ordering::Acquire)
                {
                    return None;
                }
                entry.clone()
            } else {
                if store.issued_process_incarnation(process_id) != Some(incarnation) {
                    return None;
                }
                let (_, response, _) = store.finished.iter().find(|(id, _, _)| *id == process_id)?;
                let mut response = response.clone();
                response.process_output = None;
                response.truncation_policy = truncation_policy;
                response.max_output_tokens = max_output_tokens;
                return Some(response);
            }
        };
        let process = &entry.process;
        let _interaction = process.interaction_lock().try_lock_owned().ok()?;
        let (snapshot, artifact) = process.try_terminal_output(entry.tty)?;
        let handles = process.output_handles();
        let mut store = self.process_store.try_lock().ok()?;
        if !store.processes.get(&process_id).is_some_and(|current| Arc::ptr_eq(&current.process, process)) {
            return None;
        }
        let mut buffer = handles.output_buffer.try_lock().ok()?;
        buffer.collect_pending_output();
        let lag_marker = buffer.pending_output().filter(|output| output.lagged_chunks() > 0)
            .map(|output| lagged_output_marker(output.lagged_chunks())).unwrap_or_default();
        let (raw_output, output_ranges) = buffer.projected_pending_output(true, &lag_marker);
        let stream_ranges = buffer.pending_stream_ranges();
        let exit_code = process.exit_code();
        let wall_time = snapshot.process_duration.unwrap_or_default();
        let response = ExecCommandToolOutput {
            stream_chunks: snapshot.stream_chunks(stream_ranges.as_ref()),
            process_output: Some(snapshot), error: process.failure_message(),
            environment_receipt: process.environment_receipt(),
            validation: process.validation(), event_call_id: entry.call_id,
            chunk_id: generate_chunk_id(), wall_time,
            original_token_count: Some(approx_token_count(&String::from_utf8_lossy(&raw_output))),
            raw_output, output_ranges, stream_ranges, truncation_policy, max_output_tokens,
            quiet_on_success: false,
            process_id: None, session_capabilities: process.shutdown_status().map(|_| process.session_capabilities(entry.tty)), exit_code, process_exited: true,
            search_no_match: entry.search_exit_one_is_no_match && exit_code == Some(1),
            hook_command: None, tool_cwd: None, raw_output_artifact: artifact, repair_notice: None,
            pending_deferred_completions: Vec::new(),
        };
        buffer.acknowledge_pending_output();
        store.remove(process_id);
        store.remember_finished(process_id, &response);
        Some(response)
    }


    async fn refresh_process_state(
        &self,
        process_id: u32,
        expected_process: &Arc<UnifiedExecProcess>,
    ) -> ProcessStatus {
        // Once output is closed under the producer's buffer lock, it cannot
        // gain more bytes. Inspect that terminal state before taking the global
        // store lock, then verify the process identity before removing it.
        let drained = if expected_process.has_exited() && expected_process.output_is_closed() {
            let handles = expected_process.output_handles();
            let buffer = handles.output_buffer.lock().await;
            Self::process_can_retire(expected_process, &buffer)
        } else {
            false
        };
        let mut store = self.process_store.lock().await;
        let Some(entry) = store.processes.get_mut(&process_id) else {
            return ProcessStatus::Unknown;
        };
        if !Arc::ptr_eq(&entry.process, expected_process) {
            return ProcessStatus::Unknown;
        }

        let exit_code = entry.process.exit_code();
        let process_id = entry.process_id;

        if drained {
            // Retirement and replay publication happen together in acknowledge_output.
            let entry = entry.clone();
            ProcessStatus::Exited {
                exit_code,
                entry: Box::new(entry),
            }
        } else if entry.process.has_exited() {
            ProcessStatus::OutputPending {
                exit_code,
                call_id: entry.call_id.clone(),
                process_id,
            }
        } else {
            ProcessStatus::Running {
                exit_code,
                call_id: entry.call_id.clone(),
                process_id,
            }
        }
    }

    fn finished_stdin_response(
        store: &ProcessStore,
        request: &WriteStdinRequest<'_>,
    ) -> Option<Result<ExecCommandToolOutput, UnifiedExecError>> {
        let mut response = store.finished_response(request)?;
        if request.close_stdin && let Ok(output) = &mut response {
            output.error = Some("EOF was not sent because the process already exited.".into());
        }
        Some(response)
    }

    /// Recovery-only receipt: the backend was not invoked and no output was
    /// consumed. Passive polls may yield successfully; mutations did not run.
    fn unadmitted_interaction_result(
        handles: &PreparedProcessHandles,
        request: &WriteStdinRequest<'_>,
        wait_time: Duration,
    ) -> ExecCommandToolOutput {
        let process_exited = handles.process.has_exited();
        let exit_code = handles.process.exit_code();
        let state = if process_exited {
            "Process exited; output or completion may remain pending."
        } else {
            "Process was still running at observation."
        };
        let error = if request.close_stdin {
            Some("EOF was not sent; the close was not dispatched.".to_string())
        } else if !request.input.is_empty() {
            Some("Input was not delivered; the interaction was not dispatched.".to_string())
        } else {
            None
        };
        let notice = if error.is_some() {
            format!(
                "Interaction admission ended after {:.3}s (deadline or cancellation). {state} Poll the retained session; do not restart the command.",
                wait_time.as_secs_f64()
            )
        } else {
            format!(
                "Interaction deadline expired after {:.3}s queued. {state} No input was requested. Poll the retained session; do not restart the command.",
                wait_time.as_secs_f64()
            )
        };
        ExecCommandToolOutput {
            stream_chunks: None,
            stream_ranges: None,
            output_ranges: None,
            process_output: None,
            environment_receipt: handles.process.environment_receipt(),
            error,
            validation: handles.process.validation(),
            event_call_id: handles.call_id.clone(),
            chunk_id: generate_chunk_id(),
            wall_time: wait_time,
            raw_output: Vec::new(),
            truncation_policy: request.truncation_policy,
            max_output_tokens: request.max_output_tokens,
            quiet_on_success: false,
            process_id: Some(handles.process_id),
            session_capabilities: Some(handles.process.session_capabilities(handles.tty)),
            exit_code,
            process_exited,
            search_no_match: handles.search_exit_one_is_no_match && exit_code == Some(1),
            original_token_count: Some(0),
            hook_command: Some(handles.hook_command.clone()),
            tool_cwd: handles.hook_cwd.clone(),
            raw_output_artifact: handles.process.try_raw_output_artifact(),
            repair_notice: Some(notice),
            pending_deferred_completions: Vec::new(),
        }
    }

    async fn prepare_process_handles(
        &self,
        process_id: u32,
        expected_process: &Arc<UnifiedExecProcess>,
    ) -> Result<PreparedProcessHandles, UnifiedExecError> {
        let mut store = self.process_store.lock().await;
        let entry = store
            .processes
            .get_mut(&process_id)
            .ok_or(UnifiedExecError::UnknownProcessId { process_id })?;
        if !Arc::ptr_eq(&entry.process, expected_process) {
            return Err(UnifiedExecError::UnknownProcessId { process_id });
        }
        entry.last_used = Instant::now();
        let OutputHandles {
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
            ..
        } = entry.process.output_handles();
        let session = entry.session.upgrade();
        let pause_state = session.as_ref().map(|session| session.subscribe_elicitation_pause_state());
        let terminal_tasks = session.as_ref().map(|session| session.terminal_tasks.clone());

        Ok(PreparedProcessHandles {
            process: Arc::clone(&entry.process),
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
            pause_state,
            call_id: entry.call_id.clone(),
            hook_command: entry.hook_command.clone(),
            hook_cwd: entry.hook_cwd.clone(),
            process_id: entry.process_id,
            search_exit_one_is_no_match: entry.search_exit_one_is_no_match,
            tty: entry.tty,
            validation_launch: entry.validation_launch,
            terminal_tasks,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_process(
        &self,
        process: Arc<UnifiedExecProcess>,
        context: &UnifiedExecContext,
        command: &[String],
        hook_command: String,
        hook_cwd: Option<PathUri>,
        cwd: PathUri,
        started_at: Instant,
        process_id: u32,
        process_id_reservation: &mut ProcessIdReservation,
        tty: bool,
        attempt_key: crate::tools::command_execution::CommandAttemptKey,
        raw_output_artifact: crate::tools::command_output_artifact::RawOutputArtifact,
            transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
        initial_exec_command_active: Arc<AtomicBool>,
        registration: &mut PendingProcessRegistration,
        validation_launch: bool,
        validation_started_at: Option<Instant>,
    ) -> Result<(), UnifiedExecError> {
        let command_execution_id = context
            .session
            .services
            .command_execution
            .allocate_execution_id();
        let tool_dispatch_timing = active_tool_dispatch_timing();
        let parent_tool_execution_id = tool_dispatch_timing
            .as_ref()
            .map(|timing| timing.execution_id().clone())
            .unwrap_or_default();
        let entry = ProcessEntry {
            process: Arc::clone(&process),
            command_execution_id,
            parent_tool_execution_id: parent_tool_execution_id.clone(),
            call_id: context.call_id.clone(),
            process_id,
            cwd: cwd.clone(),
            initial_exec_command_active,
            search_exit_one_is_no_match: attempt_key.is_search_no_match(Some(1))
                || (!validation_launch
                    && process.validation().is_none()
                    && crate::tools::shell_output_summary::ends_with_native_search(&hook_command)),
            hook_command,
            hook_cwd,
            tty,
            validation_launch,
            session: Arc::downgrade(&context.session),
            last_used: started_at,
        };
        let classified_search = entry.search_exit_one_is_no_match;
        {
            let store = self.process_store.lock().await;
            // Bind the exact output/replay identity before ledger promotion, but
            // keep the reservation provisional until the watcher can own its End.
            process.bind_managed_incarnation(store.process_id_history.incarnation(process_id));
        }

        if let Err(error) = context
            .session
            .services
            .command_execution
            .track_running_process_with_execution_id(
                command_execution_id,
                parent_tool_execution_id.clone(),
                process_id,
                attempt_key,
                raw_output_artifact.clone(),
                process.session_capabilities(tty).incarnation,
            )
            .await
        {
            // The accepted Begin and provisional process belong to the tracked
            // startup owner when ledger promotion cannot finish.
            return Err(UnifiedExecError::process_failed(error));
        }

        {
            let mut store = self.process_store.lock().await;
            let append = BackgroundTerminalAppend::enroll(
                &self.pending_cleanup_owners,
                process_id,
                process.incarnation(),
            )?;
            // There is no await between enrolling the exact End and making the
            // live entry visible to pollers, pruning, or any cleanup route.
            registration.pending_startup_completion = None;
            spawn_exit_watcher(
                Arc::clone(&process),
                Arc::clone(&context.session),
                Arc::clone(&context.turn),
                context.call_id.clone(),
                command.to_vec(),
                cwd,
                process_id,
                command_execution_id,
                parent_tool_execution_id,
                transcript,
                started_at,
                context.source.clone(),
                context.tracker.clone(),
                tool_dispatch_timing,
                tty,
                classified_search,
                validation_started_at,
                append,
            );
            let reserved = store.reserved_process_slots.remove(&process_id);
            debug_assert!(reserved, "process capacity must be reserved before launch");
            store.processes.insert(process_id, entry);
            process_id_reservation.transfer_to_store();
        }
        registration.commit().await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open_session_with_exec_env(
        &self,
        process_id: u32,
        command: SandboxCommand,
        options: ExecOptions,
        additional_permissions_uri: Option<
            &codex_protocol::request_permissions::UriAdditionalPermissionProfile,
        >,
        attempt: &SandboxAttempt<'_>,
        exec_server_env_config: Option<ExecServerEnvConfig>,
        tty: bool,
        pipe_stdin: bool,
        spawn_lifecycle: SpawnLifecycleHandle,
        raw_output_artifact: Option<crate::tools::command_output_artifact::RawOutputArtifact>,
        environment: &codex_exec_server::Environment,
        exec_backend: Option<Arc<dyn codex_exec_server::ExecBackend>>,
        pending_spawns: &PendingSpawnRegistration,
    ) -> Result<Arc<UnifiedExecProcess>, ToolError> {
        let setup_phase = crate::tools::tool_dispatch_trace::begin_tool_phase("sandbox_setup");
        let setup_timing = active_tool_dispatch_timing()
            .and_then(|timing| timing.turn_timing_state())
            .map(|timing| timing.begin_local_phase(TurnLocalPhase::ExecutorReadinessWait));
        let mut request = if environment.is_remote() {
            attempt.env_for_exec_server(
                command,
                options,
                additional_permissions_uri,
            )
        } else {
            attempt
                .env_for(command, options)
                .await
        }
        .map_err(ToolError::Codex)?;
        request.exec_server_env_config = exec_server_env_config;
        drop(setup_timing);
        drop(setup_phase);
        self.open_session_with_prepared_exec_env(
            process_id,
            &request,
            tty,
            pipe_stdin,
            spawn_lifecycle,
            raw_output_artifact,
            environment,
            exec_backend,
            pending_spawns,
        )
        .await
        .map_err(|err| match err {
            UnifiedExecError::SandboxDenied { output, .. } => {
                ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                    output,
                    network_policy_decision: None,
                }))
            }
            other => ToolError::Codex(CodexErr::Io(std::io::Error::other(other))),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open_session_with_prepared_exec_env(
        &self,
        process_id: u32,
        request: &ExecRequest,
        tty: bool,
        pipe_stdin: bool,
        mut spawn_lifecycle: SpawnLifecycleHandle,
        raw_output_artifact: Option<crate::tools::command_output_artifact::RawOutputArtifact>,
        environment: &codex_exec_server::Environment,
        exec_backend: Option<Arc<dyn codex_exec_server::ExecBackend>>,
        pending_spawns: &PendingSpawnRegistration,
    ) -> Result<Arc<UnifiedExecProcess>, UnifiedExecError> {
        let spawn_phase = crate::tools::tool_dispatch_trace::begin_tool_phase("process_spawn");
        if let Some(dispatch) = active_tool_dispatch_timing()
            && let Some(timing) = dispatch.turn_timing_state()
            && timing.runtime_async_observations_enabled()
        {
            let effective_env = request.env.iter().collect::<std::collections::BTreeMap<_, _>>();
            let command = codex_conversation::operation_history::digest(serde_json::json!({
                "command":request.command, "cwd":request.cwd, "environment":effective_env,
                "arg0":request.arg0, "tty":tty, "pipe_stdin":pipe_stdin}));
            let configuration = codex_conversation::operation_history::digest(serde_json::json!({
                "permissions":request.permission_profile,
                "private_desktop":request.windows_sandbox_private_desktop,
                "workspace_roots":request.windows_sandbox_workspace_roots}));
            // This sample precedes spawn but is not a durability gate.
            timing.observe_runtime_queued("command_prepared", &dispatch.execution_id().0,
                serde_json::json!({"command_sha256":command, "configuration_sha256":configuration,
                    "process_handle":process_id,
                    "identity_scope":"prepared_host_command_arguments_cwd_environment_and_policy",
                    "input_state_complete":false,
                    "remote_final_environment_complete":!environment.is_remote()}));
        }
        let spawn_timing = active_tool_dispatch_timing()
            .and_then(|timing| timing.turn_timing_state())
            .map(|timing| timing.begin_local_phase(TurnLocalPhase::ExecutorReadinessWait));
        let inherited_fds = spawn_lifecycle.inherited_fds();

        if environment.is_remote() {
            if !inherited_fds.is_empty() {
                return Err(UnifiedExecError::create_process(
                    "remote exec-server does not support inherited file descriptors".to_string(),
                ));
            }

            let params = exec_server_params_for_request(process_id, request, tty, pipe_stdin).await?;
            let started = exec_backend
                .unwrap_or_else(|| environment.get_exec_backend())
                .start(params)
                .await
                .map_err(|error| {
                    if matches!(
                        &error,
                        codex_exec_server::ExecServerError::ProcessStart { .. }
                    ) {
                        UnifiedExecError::RemoteStart {
                            error: Box::new(error),
                        }
                    } else {
                        UnifiedExecError::create_process(error.to_string())
                    }
                })?;
            codex_utils_pty::process_observation::observe_process_start(
                "remote_exec",
                codex_utils_pty::process_observation::ProcessRole::RequestedRoot,
                None,
                Some(format!("{:?}", started.process.process_id())),
            );
            spawn_lifecycle.after_spawn();
            mark_exec_process_spawned();
            drop(spawn_timing);
            drop(spawn_phase);
            return UnifiedExecProcess::from_exec_server_started(
                started,
                raw_output_artifact,
                pending_spawns,
            )
            .await;
        }

        // TODO(anp): Keep PathUri through the local PTY/process launch boundary.
        let native_cwd = request
            .cwd
            .to_abs_path()
            .map_err(|_| UnifiedExecError::ForeignPath {
                path: request.cwd.clone(),
            })?;

        let (program, args) = request
            .command
            .split_first()
            .ok_or(UnifiedExecError::MissingCommandLine)?;
        // Speculative shell prototype: only a local, noninteractive launch with
        // null stdin and no inherited handles can use a matching prepared host.
        let speculative = if !tty && !pipe_stdin && inherited_fds.is_empty() {
            crate::unified_exec::speculative_shell::SpeculativeShellKey::for_launch(
                &request.command,
                native_cwd.as_path(),
                &request.env,
                &request.arg0,
            )
        } else {
            None
        };
        let handed_off = match speculative.as_ref() {
            Some(launch) => self
                .speculative_shell
                .take_and_hand_off(launch)
                .await
                // An unconfirmed commit is not a retryable process-creation error.
                .map_err(|error| UnifiedExecError::process_failed(error.to_string()))?,
            None => None,
        };
        let spawn_result = if let Some(spawned) = handed_off {
            Ok(spawned)
        } else if tty {
            codex_utils_pty::pty::spawn_process_with_inherited_fds(
                program,
                args,
                native_cwd.as_path(),
                &request.env,
                &request.arg0,
                codex_utils_pty::TerminalSize::default(),
                &inherited_fds,
            )
            .await
        } else if pipe_stdin {
            codex_utils_pty::pipe::spawn_process_with_inherited_fds(
                program,
                args,
                native_cwd.as_path(),
                &request.env,
                &request.arg0,
                &inherited_fds,
            )
            .await
        } else {
            codex_utils_pty::pipe::spawn_process_no_stdin_with_inherited_fds(
                program,
                args,
                native_cwd.as_path(),
                &request.env,
                &request.arg0,
                &inherited_fds,
            )
            .await
        };
        let spawned = spawn_result.map_err(|error| {
            #[cfg(windows)]
            if let Some(cause) = error
                .chain()
                .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                .find(|io| {
                    io.raw_os_error()
                        == Some(windows_sys::Win32::Foundation::ERROR_FILENAME_EXCED_RANGE as i32)
                })
            {
                return UnifiedExecError::LaunchInputRefused {
                    message: format!(
                        "Windows could not launch the process: {cause}. Shorten the command line or paths before trying again."
                    ),
                    kind: "windows_launch_length",
                };
            }
            #[cfg(windows)]
            if let Some(refusal) = error.downcast_ref::<codex_utils_pty::pipe::PipeSpawnError>() {
                return UnifiedExecError::CreateProcess {
                    message: error.to_string(),
                    refusal: Some(refusal.0.kind()),
                };
            }
            UnifiedExecError::create_process(error.to_string())
        })?;
        if let Some(launch) = speculative {
            self.speculative_shell.replenish(launch.key);
        }
        spawn_lifecycle.after_spawn();
        mark_exec_process_spawned();
        drop(spawn_timing);
        drop(spawn_phase);
        UnifiedExecProcess::from_spawned(
            spawned,
            codex_sandboxing::SandboxType::None,
            spawn_lifecycle,
            raw_output_artifact,
            pending_spawns,
        )
        .await
    }

    pub(super) async fn open_session_with_sandbox(
        &self,
        request: &ExecCommandRequest,
        cwd: PathUri,
        context: &UnifiedExecContext,
        pending_spawns: PendingSpawnRegistration,
    ) -> Result<Arc<UnifiedExecProcess>, UnifiedExecError> {
        let (env, local_policy_env) = build_unified_exec_environment(context);
        let exec_server_env_config = ExecServerEnvConfig {
            policy: exec_env_policy_from_shell_policy(
                &context.turn.config.permissions.shell_environment_policy,
            ),
            local_policy_env,
        };
        let mut orchestrator = ToolOrchestrator::new();
        let mut runtime = UnifiedExecRuntime::new_with_pending_spawns(self, pending_spawns);

        let direct_argv_analysis = async {
            if request.shell_wrapper_is_owned
                && request.shell_type == crate::shell::ShellType::PowerShell
            {
                match request.normalization_cwd.as_ref() {
                    Some(cwd) => {
                        prove_noprofile_powershell_direct_argv_async(
                            &request.command_for_safety,
                            cwd,
                            &env,
                        )
                        .await
                    }
                    None => None,
                }
            } else {
                None
            }
        };

        let canonical_approval = async {
            let proven_direct_argv = direct_argv_analysis.await;
            let canonical_exec_approval_requirement =
                if let Some(proof) = proven_direct_argv.as_ref() {
                    Some(
                        context
                            .session
                            .services
                            .exec_policy
                            .create_exec_approval_requirement_for_command(ExecApprovalRequest {
                                command: proof.command_for_policy(),
                                command_for_safety: None,
                                execution_context: request.normalization_cwd.as_deref().map(|cwd| (cwd, &env)),
                                approval_policy: context.turn.approval_policy.value(),
                                permission_profile: crate::tools::sandboxing::command_permission_profile(&context.turn),
                                windows_sandbox_level: crate::tools::sandboxing::command_windows_sandbox_level(&context.turn),
                                sandbox_permissions: if request.additional_permissions_preapproved {
                                    crate::sandboxing::SandboxPermissions::UseDefault
                                } else {
                                    request.sandbox_permissions
                                },
                                prefix_rule: None,
                            })
                            .await,
                    )
                } else {
                    None
                };
            (proven_direct_argv, canonical_exec_approval_requirement)
        };
        let exec_approval_request = ExecApprovalRequest {
            command: &request.command,
            command_for_safety: Some(&request.command_for_safety),
            execution_context: request.normalization_cwd.as_deref().map(|cwd| (cwd, &env)),
            approval_policy: context.turn.approval_policy.value(),
            permission_profile: crate::tools::sandboxing::command_permission_profile(&context.turn),
            windows_sandbox_level: crate::tools::sandboxing::command_windows_sandbox_level(&context.turn),
            sandbox_permissions: if request.additional_permissions_preapproved {
                crate::sandboxing::SandboxPermissions::UseDefault
            } else {
                request.sandbox_permissions
            },
            prefix_rule: request.prefix_rule.clone(),
        };
        let original_approval = async {
            if request.shell_wrapper_is_owned {
                context
                    .session
                    .services
                    .exec_policy
                    .create_exec_approval_requirement_for_command(exec_approval_request)
                    .await
            } else {
                context
                    .session
                    .services
                    .exec_policy
                    .create_exec_approval_requirement_for_direct_argv(exec_approval_request)
                    .await
            }
        };
        // The canonical policy still depends on the direct-argv proof. Only
        // the independent original command policy runs alongside that chain.
        let (
            (proven_direct_argv, canonical_exec_approval_requirement),
            mut exec_approval_requirement,
        ) = tokio::join!(canonical_approval, original_approval);

        // An argv safelist cannot prove a command trustworthy when its loader,
        // configuration or executable search path is supplied by the caller.
        if !context.env_overrides.is_empty()
            && context.turn.approval_policy.value()
                == codex_protocol::protocol::AskForApproval::UnlessTrusted
            && matches!(
                exec_approval_requirement,
                crate::tools::sandboxing::ExecApprovalRequirement::Skip { .. }
            )
        {
            exec_approval_requirement = crate::tools::sandboxing::ExecApprovalRequirement::NeedsApproval {
                reason: Some("Command-scoped environment overrides require approval under the untrusted-command policy.".into()),
                proposed_execpolicy_amendment: None,
            };
        }

        let approved_powershell_direct_argv = if let (Some(proof), Some(canonical_requirement)) =
            (proven_direct_argv, canonical_exec_approval_requirement)
            && same_exec_authorization_envelope(&exec_approval_requirement, &canonical_requirement)
            && let Some(cwd) = request.normalization_cwd.as_ref()
            && let Some(command) =
                proof.into_command_for_state(&request.command_for_safety, cwd, &env)
        {
            Some(command)
        } else {
            None
        };
        let req = UnifiedExecToolRequest {
            command: request.command.clone(),
            command_for_approval: request.command_for_safety.clone(),
            shell_wrapper_is_owned: request.shell_wrapper_is_owned,

            normalization_cwd: request.normalization_cwd.clone(),

            approved_powershell_direct_argv,
            raw_output_artifact: request.raw_output_artifact.clone(),
            shell_type: request.shell_type,
            hook_command: request.hook_command.clone(),
            process_id: request.process_id,
            cwd,
            turn_environment: request.turn_environment.clone(),
            env,
            exec_server_env_config: Some(exec_server_env_config),
            explicit_env_overrides: context.explicit_env_overrides(),
            has_command_env_overrides: !context.env_overrides.is_empty(),
            tty: request.tty,
            pipe_stdin: request.pipe_stdin,
            sandbox_permissions: request.sandbox_permissions,
            additional_permissions: request.additional_permissions.clone(),
            additional_permissions_uri: request.additional_permissions_uri.clone(),
            justification: request.justification.clone(),
            exec_approval_requirement,
        };
        let tool_ctx = ToolCtx {
            session: context.session.clone(),
            turn: context.turn.clone(),
            call_id: context.call_id.clone(),
            tool_name: ToolName::plain("exec_command"),
        };
        orchestrator
            .run(
                &mut runtime,
                &req,
                &tool_ctx,
                &context.turn,
                context.turn.approval_policy.value(),
            )
            .await
            .map(|result| result.output)
            .map_err(|err| match err {
                ToolError::Denied(message) => UnifiedExecError::PermissionDenied { message },
                ToolError::Codex(CodexErr::Io(error))
                    if error.get_ref().is_some_and(<dyn std::error::Error + std::marker::Send + std::marker::Sync + 'static>::is::<UnifiedExecError>) =>
                {
                    match error.into_inner().and_then(|source| source.downcast::<UnifiedExecError>().ok()) {
                        Some(error) => *error,
                        None => UnifiedExecError::process_failed("remote start evidence became unavailable".into()),
                    }
                }
                ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied { output, .. })) => {
                    let output = *output;
                    let message = if output.aggregated_output.text.is_empty() {
                        let exit_code = output.exit_code;
                        format!("Process exited with code {exit_code}")
                    } else {
                        output.aggregated_output.text.clone()
                    };
                    UnifiedExecError::sandbox_denied(message, output)
                }
                other => UnifiedExecError::create_process(format!("{other:?}")),
            })
    }




    #[allow(clippy::too_many_arguments)]
    async fn collect_output_until_deadline_with_quiet_yield(
        output_buffer: &OutputBuffer,
        output_notify: &Arc<Notify>,
        output_closed: &Arc<AtomicBool>,
        output_closed_notify: &Arc<Notify>,
        cancellation_token: &CancellationToken,
        mut pause_state: Option<watch::Receiver<bool>>,
        mut deadline: Instant,
        hard_deadline: Option<Instant>,
        quiet_period: Option<Duration>,
        mut cold_start_allowance: Duration,
        handoff: &mut Option<std::pin::Pin<Box<tokio::sync::futures::OwnedNotified>>>,
    ) -> CollectedOutput {
        output_buffer.lock().await.begin_output_report();
        // Draining frees producer capacity, so the entire collection window needs
        // its own bound. Raw output is still captured by the process artifact writer.
        let tool_dispatch_timing = active_tool_dispatch_timing();
        let mut wait_attempt = 0_u32;
        let mut exit_signal_received = cancellation_token.is_cancelled();
        let mut post_exit_deadline: Option<Instant> = None;
        // Silence alone is not progress. Honor the requested deadline until
        // meaningful output starts the shorter quiet-period deadline.
        let mut early_yield_deadline = None;
        // Newest output, which shows a cold shell is past startup.
        let mut last_output_at: Option<Instant> = None;
        let wake_reason = loop {
            tokio::select! {
                biased;
                _ = Self::wait_for_interaction(handoff) => break ToolLifecycleWakeReason::Retry,
                _ = async {
                    match hard_deadline {
                        Some(bound) => tokio::time::sleep_until(bound).await,
                        None => std::future::pending::<()>().await,
                    }
                } => break ToolLifecycleWakeReason::Timeout,
                _ = Self::extend_deadlines_while_paused(
                    &mut pause_state,
                    &mut deadline,
                    &mut post_exit_deadline,
                    &mut early_yield_deadline,
                ) => {}
            }
            if let Some(bound) = hard_deadline {
                deadline = deadline.min(bound);
            }
            // Register before inspecting the buffer. `notify_waiters` does not
            // retain a permit, so registering after an empty drain would leave
            // a race where output can arrive between the drain and the first
            // poll of `notified()`, stranding an owner-held wait until its
            // deadline.
            let output_notified = output_notify.notified();
            tokio::pin!(output_notified);
            output_notified.as_mut().enable();
            let closed = output_closed_notify.notified();
            tokio::pin!(closed);
            closed.as_mut().enable();
            let (output_arrived, meaningful_output_arrived, output_was_closed) = {
                let mut guard = output_buffer.lock().await;
                // Observe closure before the drain under the producer's buffer
                // lock. If it is closed, this drain includes every final byte.
                let closed = output_closed.load(Ordering::Acquire);
                let arrived = guard.has_uncollected_output();
                let meaningful = guard.collect_pending_output();
                (arrived, meaningful && quiet_period.is_some(), closed)
            };
            if output_arrived {
                last_output_at = Some(Instant::now());
            }

            if !output_arrived {
                exit_signal_received |= cancellation_token.is_cancelled();
                if exit_signal_received && output_was_closed {
                    break ToolLifecycleWakeReason::Completed;
                }
                let effective_deadline = if exit_signal_received {
                    deadline
                } else {
                    early_yield_deadline
                        .map(|early_deadline| early_deadline.min(deadline))
                        .unwrap_or(deadline)
                };
                let remaining = effective_deadline.saturating_duration_since(Instant::now());
                if remaining == Duration::ZERO {
                    if effective_deadline >= deadline
                        && !exit_signal_received
                        && let Some(step) = Self::cold_start_extension(
                            cold_start_allowance,
                            last_output_at,
                            deadline,
                            hard_deadline,
                        )
                    {
                        deadline += step;
                        cold_start_allowance -= step;
                        continue;
                    }
                    break if effective_deadline < deadline {
                        ToolLifecycleWakeReason::Completed
                    } else {
                        ToolLifecycleWakeReason::Timeout
                    };
                }

                if exit_signal_received {
                    let now = Instant::now();
                    let close_wait_deadline = *post_exit_deadline.get_or_insert_with(|| {
                        let period = quiet_period.unwrap_or(INITIAL_OUTPUT_QUIET_PERIOD);
                        (now + period).min(deadline)
                    });
                    let close_wait_remaining = close_wait_deadline.saturating_duration_since(now);
                    if close_wait_remaining == Duration::ZERO {
                        break ToolLifecycleWakeReason::Timeout;
                    }
                    if wait_attempt > 0
                        && let Some(timing) = tool_dispatch_timing.as_ref()
                    {
                        // Re-entering an output wait does not retry the command.
                        timing.increment_reentry_count();
                    }
                    wait_attempt = wait_attempt.saturating_add(1);
                    let requested_timeout_ms =
                        u64::try_from(close_wait_remaining.as_millis()).unwrap_or(u64::MAX);
                    let lifecycle_deadline_at_ms = tool_dispatch_timing
                        .as_ref()
                        .and_then(|timing| timing.deadline_after_ms(requested_timeout_ms));
                    if let Some(turn_timing) = tool_dispatch_timing
                        .as_ref()
                        .and_then(|timing| timing.turn_timing_state())
                    {
                        turn_timing
                            .record_tool_sample_block_reason_async(
                                NextSampleBlockReason::WaitingForProcessCleanup,
                            )
                            .await;
                    }
                    let wake_reason = tokio::select! {
                        _ = Self::wait_for_interaction(handoff) => break ToolLifecycleWakeReason::Retry,
                        _ = &mut output_notified => ToolLifecycleWakeReason::Completed,
                        _ = &mut closed => ToolLifecycleWakeReason::Completed,
                        _ = tokio::time::sleep_until(close_wait_deadline) => ToolLifecycleWakeReason::Timeout,
                        _ = Self::wait_for_pause_change(&mut pause_state) => ToolLifecycleWakeReason::Retry,
                    };
                    if let Some(timing) = tool_dispatch_timing.as_ref() {
                        timing.record_timer_wait(ToolLifecycleTimerWait {
                            wait_kind: "post_exit_output_drain".to_string(),
                            requested_timeout_ms: Some(requested_timeout_ms),
                            effective_timeout_ms: Some(requested_timeout_ms),
                            deadline_at_ms: lifecycle_deadline_at_ms,
                            wake_reason,
                            sequence: 0,
                        });
                    }
                    if wake_reason == ToolLifecycleWakeReason::Timeout {
                        break wake_reason;
                    }
                    continue;
                }

                let exit_notified = cancellation_token.cancelled();
                tokio::pin!(exit_notified);
                if wait_attempt > 0
                    && let Some(timing) = tool_dispatch_timing.as_ref()
                {
                    timing.increment_reentry_count();
                }
                wait_attempt = wait_attempt.saturating_add(1);
                let requested_timeout_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
                let lifecycle_deadline_at_ms = tool_dispatch_timing
                    .as_ref()
                    .and_then(|timing| timing.deadline_after_ms(requested_timeout_ms));
                if let Some(turn_timing) = tool_dispatch_timing
                    .as_ref()
                    .and_then(|timing| timing.turn_timing_state())
                {
                    turn_timing
                        .record_tool_sample_block_reason_async(NextSampleBlockReason::WaitingForTool)
                        .await;
                }
                let wake_reason = tokio::select! {
                        _ = Self::wait_for_interaction(handoff) => break ToolLifecycleWakeReason::Retry,
                    _ = &mut output_notified => ToolLifecycleWakeReason::Completed,
                    _ = &mut exit_notified => {
                        exit_signal_received = true;
                        ToolLifecycleWakeReason::Completed
                    },
                    _ = tokio::time::sleep_until(effective_deadline) => ToolLifecycleWakeReason::Timeout,
                    _ = Self::wait_for_pause_change(&mut pause_state) => ToolLifecycleWakeReason::Retry,
                };
                if let Some(timing) = tool_dispatch_timing.as_ref() {
                    timing.record_timer_wait(ToolLifecycleTimerWait {
                        wait_kind: "owner_output_wait".to_string(),
                        requested_timeout_ms: Some(requested_timeout_ms),
                        effective_timeout_ms: Some(requested_timeout_ms),
                        deadline_at_ms: lifecycle_deadline_at_ms,
                        wake_reason,
                        sequence: 0,
                    });
                }
                if wake_reason == ToolLifecycleWakeReason::Timeout {
                    if effective_deadline >= deadline
                        && !exit_signal_received
                        && let Some(step) = Self::cold_start_extension(
                            cold_start_allowance,
                            last_output_at,
                            deadline,
                            hard_deadline,
                        )
                    {
                        deadline += step;
                        cold_start_allowance -= step;
                        continue;
                    }
                    break if effective_deadline < deadline {
                        ToolLifecycleWakeReason::Completed
                    } else {
                        wake_reason
                    };
                }
                continue;
            }

            if meaningful_output_arrived {
                let quiet_deadline = Instant::now() + quiet_period.unwrap_or_default();
                // A quiet period may outlast the requested deadline within a
                // cold start's allowance; the deadline itself extends to meet it.
                early_yield_deadline =
                    Some(quiet_deadline.min(deadline + cold_start_allowance));
            }

            exit_signal_received |= cancellation_token.is_cancelled();
            if exit_signal_received && output_arrived {
                // Pipe closure can lag process exit on Windows. Give each
                // post-exit output burst one quiet period, rather than falling
                // back to the caller's full initial yield deadline.
                post_exit_deadline = None;
            }
            let effective_deadline = if exit_signal_received {
                deadline
            } else {
                early_yield_deadline
                    .map(|early_deadline| early_deadline.min(deadline))
                    .unwrap_or(deadline)
            };
            if Instant::now() >= effective_deadline {
                if effective_deadline >= deadline
                    && !exit_signal_received
                    && let Some(step) = Self::cold_start_extension(
                        cold_start_allowance,
                        last_output_at,
                        deadline,
                        hard_deadline,
                    )
                {
                    deadline += step;
                    cold_start_allowance -= step;
                    continue;
                }
                break if effective_deadline < deadline {
                    ToolLifecycleWakeReason::Completed
                } else {
                    ToolLifecycleWakeReason::Timeout
                };
            }
        };

        let mut guard = output_buffer.lock().await;
        // A handoff can win the select before the ordinary output drain. Include
        // its diagnostic and any bytes already committed by the producer.
        guard.collect_pending_output();
        let lag_marker = guard
            .pending_output()
            .map(|collected| {
                if collected.lagged_chunks() > 0 {
                    lagged_output_marker(collected.lagged_chunks())
                } else {
                    Vec::new()
                }
            })
            .unwrap_or_default();
        let (output, ranges) =
            guard.projected_pending_output(output_closed.load(Ordering::Acquire), &lag_marker);
        CollectedOutput {
            bytes: output,
            stream_ranges: guard.pending_stream_ranges(),
            ranges,
            wake_reason,
            // Unspent cold-start allowance still bounds the finalizers, so
            // ending the wait at the requested time does not shorten them.
            observation_deadline: {
                let bound_for_finalizers = deadline + cold_start_allowance;
                hard_deadline.map_or(bound_for_finalizers, |bound| bound_for_finalizers.min(bound))
            },
        }
    }

    /// Windows cold-start protection for the first command in an environment:
    /// how much of the unspent allowance to wait now that the requested
    /// `deadline` has passed with the process alive. A shell that has shown no
    /// output may still be starting, so it keeps the whole allowance. Output
    /// still arriving within one quiet period may be about to end (shell exit
    /// follows its last output), so wait out that period. A process that
    /// printed and went quiet has proven the shell is up and yields now.
    fn cold_start_extension(
        allowance: Duration,
        last_output_at: Option<Instant>,
        deadline: Instant,
        hard_deadline: Option<Instant>,
    ) -> Option<Duration> {
        if allowance.is_zero() || hard_deadline.is_some_and(|bound| deadline >= bound) {
            return None;
        }
        match last_output_at {
            None => Some(allowance),
            Some(last_output_at) => (last_output_at + INITIAL_OUTPUT_QUIET_PERIOD)
                .checked_duration_since(deadline)
                .filter(|wait| !wait.is_zero())
                .map(|wait| wait.min(allowance)),
        }
    }

    async fn wait_for_interaction(
        handoff: &mut Option<std::pin::Pin<Box<tokio::sync::futures::OwnedNotified>>>,
    ) {
        if let Some(notified) = handoff.as_mut() {
            notified.await;
        } else {
            std::future::pending::<()>().await;
        }
    }

    async fn extend_deadlines_while_paused(
        pause_state: &mut Option<watch::Receiver<bool>>,
        deadline: &mut Instant,
        post_exit_deadline: &mut Option<Instant>,
        early_yield_deadline: &mut Option<Instant>,
    ) {
        let Some(receiver) = pause_state.as_mut() else {
            return;
        };
        if !*receiver.borrow_and_update() {
            return;
        }

        let paused_at = Instant::now();
        while *receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                *pause_state = None;
                break;
            }
        }

        let paused_for = paused_at.elapsed();
        *deadline += paused_for;
        if let Some(post_exit_deadline) = post_exit_deadline.as_mut() {
            *post_exit_deadline += paused_for;
        }
        if let Some(early_yield_deadline) = early_yield_deadline.as_mut() {
            *early_yield_deadline += paused_for;
        }
    }

    async fn wait_for_pause_change(pause_state: &mut Option<watch::Receiver<bool>>) {
        if let Some(receiver) = pause_state.as_mut() {
            if receiver.changed().await.is_ok() {
                return;
            }
            // A closed watch is permanently ready. Stop observing it so output
            // and the existing deadline can wake this wait without a busy loop.
            *pause_state = None;
        }
        std::future::pending::<()>().await;
    }

    fn process_can_retire(process: &UnifiedExecProcess, buffer: &HeadTailBuffer) -> bool {
        process.has_exited()
            && !process.shutdown_is_pending()
            && process.output_is_closed()
            && process.terminal_completion_is_ready()
            && !buffer.has_unreported_output()
    }

    fn prune_processes_if_needed(store: &mut ProcessStore) -> Option<ProcessEntry> {
        if store.occupied_slots() < MAX_UNIFIED_EXEC_PROCESSES {
            return None;
        }

        let mut meta: Vec<(u32, Instant, bool)> = store
            .processes
            .iter()
            .filter(|(_, entry)| !entry.initial_exec_command_active.load(Ordering::Acquire))
            .map(|(id, entry)| (*id, entry.last_used, entry.process.has_exited()))
            .collect();

        while let Some(process_id) = Self::process_id_to_prune_from_meta(&meta) {
            // Do not prune a process while write_stdin owns its interaction lock.
            if let Some(interaction_lock) = store
                .processes
                .get(&process_id)
                .map(|entry| entry.process.interaction_lock())
                && let Ok(_interaction_guard) = interaction_lock.try_lock_owned()
            {
                let process = Arc::clone(&store.processes[&process_id].process);
                let handles = process.output_handles();
                // Admission must not block on output collection. In particular,
                // exit alone does not mean the final response is recoverable.
                if let Ok(buffer) = handles.output_buffer.try_lock()
                    && Self::process_can_retire(&process, &buffer)
                {
                    return store.remove(process_id);
                }
            }
            meta.retain(|(id, _, _)| *id != process_id);
        }

        None
    }

    // Centralized pruning policy so we can easily swap strategies later.
    fn process_id_to_prune_from_meta(meta: &[(u32, Instant, bool)]) -> Option<u32> {
        if meta.is_empty() {
            return None;
        }

        let mut lru = meta.to_vec();
        lru.sort_by_key(|(_, last_used, _)| *last_used);
        lru.into_iter()
            .find(|(_, _, exited)| *exited)
            .map(|(process_id, _, _)| process_id)
    }

    pub(crate) async fn terminate_all_processes(&self) -> Result<(), String> {
        let processes: Vec<(u32, Arc<UnifiedExecProcess>)> = {
            let processes = self.process_store.lock().await;
            // In-flight admissions retain their reservations until their own
            // cancellation/transfer boundary; termination only owns stored entries.
            processes
                .processes
                .iter()
                .map(|(process_id, entry)| {
                    entry.process.request_cancellation();
                    (*process_id, Arc::clone(&entry.process))
                })
                .collect()
        };

        let termination_results = futures::future::join_all(processes.into_iter().map(
            |(process_id, process)| async move {
                let result = self.terminate_stored_process(process_id, &process).await;
                (process_id, process, result)
            },
        ))
        .await;

        let mut failures = Vec::new();
        for (process_id, process, result) in termination_results {
            if let Err(err) = result {
                failures.push(format!("process {process_id} termination is unconfirmed: {err}"));
                tracing::warn!(
                    process_id,
                    %err,
                    "retaining unified exec process after unconfirmed shutdown termination"
                );
                continue;
            }
            let entry = {
                let mut store = self.process_store.lock().await;
                let is_same_process = store
                    .processes
                    .get(&process_id)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.process, &process));
                is_same_process.then(|| store.remove(process_id)).flatten()
            };
            drop(entry);
        }
        let pending = self
            .pending_cleanup_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|owner| match owner {
                PendingProcessOwner::Startup(cleanup) => Some(Arc::clone(cleanup)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !pending.is_empty() {
            failures.push("cancelled startup cleanup remains owned and pending".to_string());
        }
        for cleanup in pending {
            // Reconciliation retries until confirmation. Shutdown observes its
            // existing owner; it must not join that unbounded recovery loop.
            if !cleanup.reconciliation_started.load(Ordering::Acquire) {
                if let Some(session) = cleanup.session.upgrade() {
                    session.terminal_tasks.spawn(run_pending_process_cleanup(cleanup));
                } else {
                    enqueue_pending_process_cleanup(cleanup);
                }
            }
        }
        if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) }
    }

    async fn terminate_stored_process(
        &self,
        process_id: u32,
        process: &UnifiedExecProcess,
    ) -> Result<(), UnifiedExecError> {
        process.terminate_confirmed().await?;
        note_background_termination(&self.pending_cleanup_owners, process_id, process);
        Ok(())
    }

    /// Run only after admission, startup cleanup and native termination have
    /// drained. A writer retirement observer may time out independently.
    pub(crate) async fn wait_for_background_terminal_appends(&self) -> Result<(), String> {
        loop {
            let pending = {
                let retained = self.pending_cleanup_owners
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let pending = retained
                    .iter()
                    .filter_map(|owner| match owner {
                        PendingProcessOwner::BackgroundAppend(pending)
                            if !pending.settled.is_cancelled() =>
                        {
                            Some(pending.settled.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if pending.is_empty() {
                    let failures = retained
                        .iter()
                        .filter_map(|owner| match owner {
                            PendingProcessOwner::BackgroundAppendFailure { message, .. } => {
                                Some(message.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    return if failures.is_empty() {
                        Ok(())
                    } else {
                        Err(failures.join("; "))
                    };
                }
                pending
            };
            futures::future::join_all(pending.iter().map(CancellationToken::cancelled)).await;
        }
    }

    /// Terminates retained processes whose originating tool calls never published a durable
    /// result. This is the abort-only ownership path: a retained process whose `exec_command`
    /// result was already persisted is deliberately not selected and remains background-owned.
    ///
    /// All selected processes must confirm termination before any owner record is removed. A
    /// failed confirmation therefore leaves the complete ownership set available for a later
    /// retry and prevents the caller from publishing a terminal receipt prematurely.
    pub(crate) async fn terminate_unpublished_processes_for_call_ids(
        &self,
        call_ids: &[String],
    ) -> Result<usize, String> {
        if call_ids.is_empty() {
            return Ok(0);
        }

        let mut targets = {
            let store = self.process_store.lock().await;
            store
                .processes
                .iter()
                .filter(|(_, entry)| call_ids.iter().any(|call_id| call_id == &entry.call_id))
                .map(|(process_id, entry)| {
                    entry.process.request_cancellation();
                    (
                        *process_id,
                        entry.call_id.clone(),
                        Arc::clone(&entry.process),
                    )
                })
                .collect::<Vec<_>>()
        };
        targets.sort_by_key(|(process_id, _, _)| *process_id);

        let results = futures::future::join_all(targets.iter().map(
            |(process_id, call_id, process)| async move {
                self.terminate_stored_process(*process_id, process).await.map_err(|error| format!(
                    "process {process_id} for unpublished call {call_id} did not confirm termination: {error}"
                ))
            },
        )).await;
        if let Some(error) = results.into_iter().find_map(Result::err) {
            return Err(error);
        }

        let removed_entries = {
            let mut store = self.process_store.lock().await;
            targets
                .iter()
                .filter_map(|(process_id, call_id, process)| {
                    let is_same_owner = store.processes.get(process_id).is_some_and(|entry| {
                        entry.call_id == *call_id && Arc::ptr_eq(&entry.process, process)
                    });
                    is_same_owner.then(|| store.remove(*process_id)).flatten()
                })
                .collect::<Vec<_>>()
        };

        for entry in &removed_entries {
            entry
                .initial_exec_command_active
                .store(false, Ordering::Release);
            if let Some(session) = entry.session.upgrade() {
                session
                    .services
                    .command_execution
                    .finish_running_process_with_execution_id(
                        entry.process_id,
                        entry.command_execution_id,
                        &entry.parent_tool_execution_id,
                        entry.process.exit_code(),
                    )
                    .await;
            }
        }

        Ok(removed_entries.len())
    }

    fn background_completion_ready(entry: &ProcessEntry) -> bool {
        !entry.tty
            && !entry.initial_exec_command_active.load(Ordering::Acquire)
            && entry.process.has_exited()
            && !entry.process.shutdown_is_pending()
            && entry.process.output_is_closed()
            && entry.process.terminal_completion_is_ready()
    }

    /// Terminal results of yielded background commands that exited before the
    /// next model-visible boundary (event-driven completion delivery).
    ///
    /// Only noninteractive processes whose launching call already returned a
    /// live handle, whose output is closed and whose terminal completion is
    /// ready qualify. Each result comes from the ordinary empty-input poll, so
    /// the process retires exactly as if the model had polled it and a later
    /// poll receives the same retained finished result. Running, interactive,
    /// mid-launch, shutting-down and currently interacting processes are left
    /// untouched for the model's own poll. The ordered commit owner supplies
    /// the batch so cancelling this future saves the already collected prefix.
    pub(crate) async fn collect_exited_background_results(
        &self,
        truncation_policy: codex_utils_output_truncation::TruncationPolicy,
        max_output_tokens: usize,
        limit: usize,
        results: &mut Vec<(u32, ExecCommandToolOutput)>,
    ) {
        let candidates = {
            let store = self.process_store.lock().await;
            let mut candidates = store
                .processes
                .values()
                .filter(|entry| {
                    Self::background_completion_ready(entry)
                        && entry.process.interaction_lock().try_lock().is_ok()
                })
                .map(|entry| (entry.last_used, entry.process_id))
                .collect::<Vec<_>>();
            candidates.sort_unstable();
            candidates.truncate(limit);
            candidates
        };
        for (_, process_id) in candidates {
            let request = WriteStdinRequest {
                process_id,
                input: "",
                close_stdin: false,
                acknowledge_only: false,
                yield_time_ms: MIN_YIELD_TIME_MS,
                max_output_tokens: Some(max_output_tokens),
                truncation_policy,
                nested_deadline: None,
            };
            // Any chunk this poll consumed must reach the model, including a
            // partial drain that still names its session for the remainder.
            match self
                .write_stdin_with_observation_mode(
                    request,
                    WriteStdinObservation::BackgroundCompletion,
                    None,
                )
                .await
            {
                Ok(output) => results.push((process_id, output)),
                Err(UnifiedExecError::ProcessFailedWithOutput { output, .. }) => {
                    results.push((process_id, *output));
                }
                Err(UnifiedExecError::ToolHistoryPersistence { output: Some(output), .. }) => {
                    results.push((process_id, *output));
                }
                Err(error) => {
                    tracing::debug!(process_id, %error, "background completion receipt unavailable");
                }
            }
        }
    }

    pub(crate) async fn list_processes(&self) -> Vec<BackgroundTerminalInfo> {
        let store = self.process_store.lock().await;
        let mut entries = store
            .processes
            .values()
            .filter(|entry| !entry.process.has_exited())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.process_id);
        entries
            .into_iter()
            .map(|entry| BackgroundTerminalInfo {
                item_id: entry.call_id.clone(),
                process_id: entry.process_id.to_string(),
                command: entry.hook_command.clone(),
                cwd: entry.cwd.clone(),
            })
            .collect()
    }

    pub(crate) async fn terminate_process(&self, process_id: u32) -> bool {
        self.terminate_process_inner(process_id, false).await
    }

    pub(crate) async fn process_incarnation(&self, process_id: u32) -> Option<uuid::Uuid> {
        let store = self.process_store.lock().await;
        store
            .processes
            .get(&process_id)
            .map(|entry| entry.process.session_capabilities(entry.tty).incarnation)
            .or_else(|| {
                store
                    .finished
                    .iter()
                    .any(|(id, _, _)| *id == process_id)
                    .then(|| store.issued_process_incarnation(process_id))
                    .flatten()
            })
    }

    /// Keep the output consumer alive so write_stdin can deliver the final bytes
    /// and retire the process through its ordinary completion path.
    pub(crate) async fn terminate_process_for_poll(&self, process_id: u32) -> bool {
        self.terminate_process_inner(process_id, true).await
    }

    async fn terminate_process_inner(&self, process_id: u32, retain_for_poll: bool) -> bool {
        let process = {
            let store = self.process_store.lock().await;
            let Some(entry) = store.processes.get(&process_id) else {
                return false;
            };
            entry.process.request_cancellation();
            Arc::clone(&entry.process)
        };

        // Root exit does not imply output custody has finished. The same
        // termination owner also aborts remaining readers/finalization.
        if self.terminate_stored_process(process_id, &process).await.is_err() {
            return false;
        }
        if retain_for_poll {
            return true;
        }

        let entry = {
            let mut store = self.process_store.lock().await;
            let Some(entry) = store.processes.get(&process_id) else {
                return true;
            };
            if !Arc::ptr_eq(&entry.process, &process) {
                return true;
            }
            if entry.initial_exec_command_active.load(Ordering::Acquire) {
                return true;
            }
            let Some(entry) = store.remove(process_id) else {
                return false;
            };
            entry
        };

        if let Some(session) = entry.session.upgrade() {
            session
                .services
                .command_execution
                .finish_running_process_with_execution_id(
                    entry.process_id,
                    entry.command_execution_id,
                    &entry.parent_tool_execution_id,
                    entry.process.exit_code(),
                )
                .await;
        }
        true
    }
}

enum ProcessStatus {
    Running {
        exit_code: Option<i32>,
        call_id: String,
        process_id: u32,
    },
    OutputPending {
        exit_code: Option<i32>,
        call_id: String,
        process_id: u32,
    },
    Exited {
        exit_code: Option<i32>,
        entry: Box<ProcessEntry>,
    },
    Unknown,
}

