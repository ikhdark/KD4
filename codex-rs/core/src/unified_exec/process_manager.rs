use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
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
use crate::exec_env::CODEX_THREAD_ID_ENV_VAR;
use crate::exec_env::create_env;
use crate::exec_env::inject_permission_profile_env;
use crate::exec_policy::ExecApprovalRequest;
use crate::sandboxing::ExecOptions;
use crate::sandboxing::ExecRequest;
use crate::sandboxing::ExecServerEnvConfig;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventStage;
use crate::tools::known_delta_store;
use crate::tools::known_delta_store::KnownDeltaExecutionObservation;
use crate::tools::network_approval::DeferredNetworkApproval;
use crate::tools::network_approval::finish_deferred_network_approval;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::runtimes::is_managed_proxy_env_var;
use crate::tools::runtimes::unified_exec::UnifiedExecLaunch;
use crate::tools::runtimes::unified_exec::UnifiedExecRequest as UnifiedExecToolRequest;
use crate::tools::runtimes::unified_exec::UnifiedExecRuntime;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;

use crate::tools::sandboxing::same_exec_authorization_envelope;
use crate::tools::tool_dispatch_trace::active_tool_dispatch_timing;
use crate::tools::tool_dispatch_trace::mark_exec_process_exited;
use crate::tools::tool_dispatch_trace::mark_exec_process_spawned;
use crate::turn_timing::TurnLocalPhase;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES;
use crate::unified_exec::MAX_YIELD_TIME_MS;
use crate::unified_exec::MIN_EMPTY_YIELD_TIME_MS;
use crate::unified_exec::MIN_YIELD_TIME_MS;
use crate::unified_exec::PendingSpawnRegistration;
use crate::unified_exec::ProcessEntry;
use crate::unified_exec::ProcessIdReservation;
use crate::unified_exec::ProcessStore;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::WriteStdinRequest;
use crate::unified_exec::async_watcher::emit_exec_end_for_unified_exec;
use crate::unified_exec::async_watcher::emit_failed_exec_end_for_unified_exec;
use crate::unified_exec::async_watcher::lagged_output_marker;
use crate::unified_exec::async_watcher::omitted_output_marker;
use crate::unified_exec::async_watcher::record_known_delta_from_process_output;
use crate::unified_exec::async_watcher::spawn_exit_watcher;
use crate::unified_exec::async_watcher::start_streaming_output;
use crate::unified_exec::async_watcher::wait_for_process_output_drain;
use crate::unified_exec::clamp_yield_time_for_readiness;
use crate::unified_exec::generate_chunk_id;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::OutputBuffer;
use crate::unified_exec::process::OutputHandles;
use crate::unified_exec::process::SpawnLifecycleHandle;
use crate::unified_exec::process::UnifiedExecProcess;
use codex_network_proxy::NetworkProxy;
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
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_path_uri::PathUri;

const UNIFIED_EXEC_ENV: [(&str, &str); 10] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("LANG", "C.UTF-8"),
    ("LC_CTYPE", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("COLORTERM", ""),
    ("PAGER", UNIFIED_EXEC_PAGER),
    ("GIT_PAGER", UNIFIED_EXEC_PAGER),
    ("GH_PAGER", UNIFIED_EXEC_PAGER),
    ("CODEX_CI", "1"),
];

#[cfg(windows)]
const UNIFIED_EXEC_PAGER: &str = "more.com";
#[cfg(not(windows))]
const UNIFIED_EXEC_PAGER: &str = "cat";
const NETWORK_ACCESS_DENIED_MESSAGE: &str =
    "Network access was denied by the Codex sandbox network proxy.";
const LATE_NETWORK_DENIAL_GRACE_PERIOD: Duration = Duration::from_millis(100);
const INITIAL_OUTPUT_QUIET_PERIOD: Duration = Duration::from_millis(250);
const INTERRUPT: &str = "\u{3}";

/// Test-only override for deterministic unified exec process IDs.
///
/// In production builds this value should remain at its default (`false`) and
/// must not be toggled.
static FORCE_DETERMINISTIC_PROCESS_IDS: AtomicBool = AtomicBool::new(false);

pub(super) fn set_deterministic_process_ids_for_tests(enabled: bool) {
    FORCE_DETERMINISTIC_PROCESS_IDS.store(enabled, Ordering::Relaxed);
}

fn deterministic_process_ids_forced_for_tests() -> bool {
    FORCE_DETERMINISTIC_PROCESS_IDS.load(Ordering::Relaxed)
}

fn should_use_deterministic_process_ids() -> bool {
    cfg!(test) || deterministic_process_ids_forced_for_tests()
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
    let local_policy_env = create_env(
        &context.turn.config.permissions.shell_environment_policy,
        /*thread_id*/ None,
    );
    let mut env = local_policy_env.clone();
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

fn exec_env_policy_from_shell_policy(
    policy: &ShellEnvironmentPolicy,
) -> codex_exec_server::ExecEnvPolicy {
    let mut exclude = policy
        .exclude
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    exclude.push(CODEX_PERMISSION_PROFILE_ENV_VAR.to_string());
    let mut r#set = policy.r#set.clone();
    r#set.retain(|key, _| !key.eq_ignore_ascii_case(CODEX_PERMISSION_PROFILE_ENV_VAR));
    codex_exec_server::ExecEnvPolicy {
        inherit: policy.inherit.clone(),
        ignore_default_excludes: policy.ignore_default_excludes,
        exclude,
        r#set,
        include_only: policy
            .include_only
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
    }
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

fn exec_server_env_for_request(
    request: &ExecRequest,
) -> (
    Option<codex_exec_server::ExecEnvPolicy>,
    HashMap<String, String>,
) {
    if let Some(exec_server_env_config) = &request.exec_server_env_config {
        let mut env =
            env_overlay_for_exec_server(&request.env, &exec_server_env_config.local_policy_env);
        if request.exec_server_managed_network.is_some() {
            for (key, value) in &request.env {
                if is_managed_proxy_env_var(key, value) {
                    env.insert(key.clone(), value.clone());
                }
            }
        }
        (Some(exec_server_env_config.policy.clone()), env)
    } else {
        (None, request.env.clone())
    }
}

fn exec_server_params_for_request(
    process_id: u32,
    request: &ExecRequest,
    tty: bool,
) -> codex_exec_server::ExecParams {
    let (env_policy, env) = exec_server_env_for_request(request);
    // Sandbox retries reuse the unified-exec ID but start a distinct executor process.
    let exec_server_process_id = if request.exec_server_sandbox.is_some() {
        format!("{process_id}-{}", Uuid::new_v4())
    } else {
        process_id.to_string()
    };
    codex_exec_server::ExecParams {
        process_id: exec_server_process_id.into(),
        argv: request.command.clone(),
        cwd: request.cwd.clone(),
        env_policy,
        env,
        tty,
        pipe_stdin: false,
        arg0: request.arg0.clone(),
        sandbox: request.exec_server_sandbox.clone(),
        enforce_managed_network: request.exec_server_enforce_managed_network,
        managed_network: request.exec_server_managed_network.clone(),
    }
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
    session: Option<Arc<crate::session::session::Session>>,
    network_approval: Option<DeferredNetworkApproval>,
    call_id: String,
    hook_command: String,
    process_id: u32,
    tty: bool,
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
    primary_process: Option<Arc<UnifiedExecProcess>>,
    network_approval: Option<DeferredNetworkApproval>,
    initial_exec_command_active: Option<Arc<AtomicBool>>,
    committed: bool,
}

#[derive(Clone)]
struct PendingProcessCleanup {
    process_store: Arc<tokio::sync::Mutex<ProcessStore>>,
    session: Arc<crate::session::session::Session>,
    attempt_key: crate::tools::command_execution::CommandAttemptKey,
    process_id: u32,
    processes: Vec<PendingProcessToTerminate>,
    primary_process: Option<Arc<UnifiedExecProcess>>,
    network_approval: Option<DeferredNetworkApproval>,
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
            pending_spawns: PendingSpawnRegistration::default(),
            primary_process: None,
            network_approval: None,
            initial_exec_command_active: None,
            committed: false,
        }
    }

    pub(super) fn pending_spawns(&self) -> PendingSpawnRegistration {
        self.pending_spawns.clone()
    }

    pub(super) fn attach_process(
        &mut self,
        process: Arc<UnifiedExecProcess>,
        network_approval: Option<DeferredNetworkApproval>,
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
        self.network_approval = network_approval;
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
                requires_confirmed_termination: !process.has_exited(),
                process,
            })
            .collect();
        PendingProcessCleanup {
            process_store: Arc::clone(&self.process_store),
            session: Arc::clone(&self.session),
            attempt_key: self.attempt_key.clone(),
            process_id: self.process_id,
            processes,
            primary_process: self.primary_process.clone(),
            network_approval: self.network_approval.clone(),
        }
    }

    async fn cleanup(&mut self) -> Result<(), String> {
        assert!(!self.committed, "cannot clean up a committed registration");
        if let Some(active) = self.initial_exec_command_active.as_ref() {
            active.store(false, Ordering::Release);
        }
        cleanup_pending_process_registration(self.cleanup_payload()).await?;
        self.committed = true;
        self.pending_spawns.clear();
        Ok(())
    }

    fn commit(&mut self) {
        assert!(
            !self.committed,
            "cannot commit a registration more than once"
        );
        assert!(
            self.primary_process.is_some(),
            "cannot commit before the primary process is attached"
        );
        self.committed = true;
        self.pending_spawns.clear();
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
        let cleanup = self.cleanup_payload();
        for pending_process in &cleanup.processes {
            pending_process.process.terminate();
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = cleanup_pending_process_registration(cleanup).await {
                    tracing::error!(%error, "failed to clean up cancelled unified exec startup");
                }
            });
        } else if let Some(sender) = pending_process_cleanup_sender() {
            if let Err(error) = sender.send(cleanup) {
                tracing::error!(
                    process_id = self.process_id,
                    %error,
                    "cannot enqueue cancelled unified exec startup cleanup"
                );
            }
        } else {
            tracing::error!(
                process_id = self.process_id,
                "unified exec cleanup worker is unavailable"
            );
        }
    }
}

fn pending_process_cleanup_sender()
-> Option<&'static std::sync::mpsc::Sender<PendingProcessCleanup>> {
    static SENDER: OnceLock<Option<std::sync::mpsc::Sender<PendingProcessCleanup>>> =
        OnceLock::new();
    SENDER
        .get_or_init(|| {
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
            let (sender, receiver) = std::sync::mpsc::channel::<PendingProcessCleanup>();
            match std::thread::Builder::new()
                .name("codex-unified-exec-cleanup".to_string())
                .spawn(move || {
                    while let Ok(cleanup) = receiver.recv() {
                        runtime.block_on(async move {
                            if let Err(error) = cleanup_pending_process_registration(cleanup).await
                            {
                                tracing::error!(
                                    %error,
                                    "failed to clean up cancelled unified exec startup"
                                );
                            }
                        });
                    }
                }) {
                Ok(_handle) => Some(sender),
                Err(error) => {
                    tracing::error!(%error, "failed to spawn unified exec cleanup worker");
                    None
                }
            }
        })
        .as_ref()
}

async fn cleanup_pending_process_registration(
    cleanup: PendingProcessCleanup,
) -> Result<(), String> {
    let mut first_termination_error = None;
    for pending_process in &cleanup.processes {
        if pending_process.requires_confirmed_termination
            && let Err(error) = pending_process.process.terminate_confirmed().await
            && first_termination_error.is_none()
        {
            first_termination_error =
                Some(format!("process termination was not confirmed: {error}"));
        }
    }
    if let Some(error) = first_termination_error {
        return Err(error);
    }

    let (removed_entry, process_id_conflict) = {
        let mut store = cleanup.process_store.lock().await;
        match (
            store.processes.get(&cleanup.process_id),
            cleanup.primary_process.as_ref(),
        ) {
            (Some(entry), Some(primary_process))
                if Arc::ptr_eq(&entry.process, primary_process) =>
            {
                (store.remove(cleanup.process_id), false)
            }
            (Some(_), Some(_)) => (None, true),
            _ => (None, false),
        }
    };
    if let Some(entry) = removed_entry.as_ref() {
        unregister_network_approval_for_entry(entry).await;
    } else if let Some(network_approval) = cleanup.network_approval.as_ref() {
        cleanup
            .session
            .services
            .network_approval
            .unregister_call(network_approval.registration_id())
            .await;
    }

    let running = cleanup
        .session
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
            .and_then(|process| process.exit_code())
            .unwrap_or(-1);
        cleanup
            .session
            .services
            .command_execution
            .finish_running_process_with_execution_id(
                cleanup.process_id,
                running.execution_id,
                &running.parent_tool_execution_id,
                Some(exit_code),
            )
            .await;
    }
    Ok(())
}

async fn unregister_network_approval_for_entry(entry: &ProcessEntry) {
    if let Some(network_approval) = entry.network_approval.as_ref()
        && let Some(session) = entry.session.upgrade()
    {
        session
            .services
            .network_approval
            .unregister_call(network_approval.registration_id())
            .await;
    }
}

async fn finish_network_approval_after_process_exit_for_entry(
    entry: &ProcessEntry,
) -> Result<(), String> {
    let session = entry.session.upgrade();
    finish_deferred_network_approval_after_process_exit_for_session(
        session.as_ref(),
        entry.network_approval.clone(),
    )
    .await
}

async fn finish_deferred_network_approval_for_session(
    session: Option<&Arc<crate::session::session::Session>>,
    deferred: Option<DeferredNetworkApproval>,
) -> Result<(), String> {
    let Some(session) = session else {
        return Ok(());
    };
    finish_deferred_network_approval(session.as_ref(), deferred)
        .await
        .map_err(network_approval_error_message)
}

fn network_approval_error_message(err: ToolError) -> String {
    match err {
        ToolError::Denied(message) | ToolError::Rejected(message) => message,
        ToolError::Codex(err) => err.to_string(),
        ToolError::ValidationSkipped(skipped) => serde_json::to_string(&skipped)
            .unwrap_or_else(|_| "validation command skipped".to_string()),
    }
}

async fn network_denial_message_for_session(
    session: Option<&Arc<crate::session::session::Session>>,
    deferred: Option<DeferredNetworkApproval>,
) -> String {
    let Some(session) = session else {
        return NETWORK_ACCESS_DENIED_MESSAGE.to_string();
    };
    match finish_deferred_network_approval(session.as_ref(), deferred).await {
        Ok(()) => NETWORK_ACCESS_DENIED_MESSAGE.to_string(),
        Err(err) => network_approval_error_message(err),
    }
}

async fn wait_for_late_network_denial(network_cancelled: Option<CancellationToken>) -> bool {
    let Some(network_cancelled) = network_cancelled else {
        return false;
    };
    if network_cancelled.is_cancelled() {
        return true;
    }

    tokio::select! {
        _ = network_cancelled.cancelled() => true,
        _ = tokio::time::sleep(LATE_NETWORK_DENIAL_GRACE_PERIOD) => false,
    }
}

async fn finish_deferred_network_approval_after_process_exit_for_session(
    session: Option<&Arc<crate::session::session::Session>>,
    deferred: Option<DeferredNetworkApproval>,
) -> Result<(), String> {
    wait_for_late_network_denial(
        deferred
            .as_ref()
            .map(DeferredNetworkApproval::cancellation_token),
    )
    .await;
    finish_deferred_network_approval_for_session(session, deferred).await
}

#[allow(clippy::too_many_arguments)]
async fn emit_failed_initial_exec_end_if_unstored(
    process_started_alive: bool,
    process: Option<&Arc<UnifiedExecProcess>>,
    context: &UnifiedExecContext,
    request: &ExecCommandRequest,
    cwd: PathUri,
    transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
    fallback_output: String,
    message: String,
    wall_time: Duration,
) {
    if process_started_alive {
        return;
    }

    emit_failed_exec_end_for_unified_exec(
        Arc::clone(&context.session),
        Arc::clone(&context.turn),
        context.call_id.clone(),
        request.command_for_safety.clone(),
        cwd,
        request.turn_environment.environment_id.clone(),
        Some(request.process_id.to_string()),
        transcript,
        fallback_output,
        match process {
            Some(process) => Some(process.snapshot_completion_output().await),
            None => None,
        },
        message,
        false,
        wall_time,
        context.tracker.clone(),
    )
    .await;
}

fn terminate_process_on_network_denial(
    process: Arc<UnifiedExecProcess>,
    session: std::sync::Weak<crate::session::session::Session>,
    deferred: DeferredNetworkApproval,
) {
    let network_cancelled = deferred.cancellation_token();
    let process_exited = process.cancellation_token();
    tokio::spawn(async move {
        let denied = tokio::select! {
            _ = network_cancelled.cancelled() => true,
            _ = process_exited.cancelled() => {
                wait_for_late_network_denial(Some(network_cancelled.clone())).await
            }
        };
        if !denied {
            return;
        }
        let session = session.upgrade();
        let message = network_denial_message_for_session(session.as_ref(), Some(deferred)).await;
        if let Err(error) = process.fail_and_terminate(message).await {
            tracing::warn!(
                %error,
                "failed to confirm unified exec termination after network denial"
            );
        }
    });
}

impl UnifiedExecProcessManager {
    pub(super) async fn fail_process_with_message(
        &self,
        process_id: u32,
        process: &Arc<UnifiedExecProcess>,
        message: String,
    ) -> UnifiedExecError {
        let message = process.failure_message().unwrap_or(message);
        match process.fail_and_terminate(message.clone()).await {
            Ok(()) => {
                let removed = {
                    let mut store = self.process_store.lock().await;
                    let is_same_process = store
                        .processes
                        .get(&process_id)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.process, process));
                    is_same_process.then(|| store.remove(process_id)).flatten()
                };
                if let Some(entry) = removed {
                    unregister_network_approval_for_entry(&entry).await;
                }
                UnifiedExecError::process_failed(message)
            }
            Err(error) => {
                tracing::warn!(
                    process_id,
                    %error,
                    "retaining unified exec process because termination was not confirmed"
                );
                UnifiedExecError::process_failed(format!(
                    "{message}; process termination was not confirmed: {error}"
                ))
            }
        }
    }

    pub(crate) fn effective_environment(
        &self,
        context: &UnifiedExecContext,
    ) -> HashMap<String, String> {
        build_unified_exec_environment(context).0
    }

    async fn allocate_process_id_value(&self) -> u32 {
        loop {
            let mut store = self.process_store.lock().await;

            let process_id = if should_use_deterministic_process_ids() {
                // test or deterministic mode
                let Some(process_id) = (1_000..=u32::MAX)
                    .find(|candidate| !store.reserved_process_ids.contains(candidate))
                else {
                    panic!("process id space exhausted");
                };
                process_id
            } else {
                // production mode → random
                rand::rng().random_range(1_000..100_000)
            };

            if store.reserved_process_ids.contains(&process_id) {
                continue;
            }

            store.reserved_process_ids.insert(process_id);
            return process_id;
        }
    }

    pub(crate) async fn reserve_process_id(&self) -> ProcessIdReservation {
        let process_id = self.allocate_process_id_value().await;
        let (transfer_sender, transfer_receiver) = tokio::sync::oneshot::channel();
        let process_store = Arc::clone(&self.process_store);
        tokio::spawn(async move {
            if transfer_receiver.await.is_err() {
                process_store
                    .lock()
                    .await
                    .reserved_process_ids
                    .remove(&process_id);
            }
        });
        ProcessIdReservation::new(process_id, transfer_sender)
    }

    #[cfg(test)]
    pub(crate) async fn allocate_process_id(&self) -> u32 {
        self.allocate_process_id_value().await
    }

    pub(crate) async fn release_process_id(&self, process_id: u32) {
        let removed = {
            let mut store = self.process_store.lock().await;
            store.remove(process_id)
        };
        if let Some(entry) = removed {
            unregister_network_approval_for_entry(&entry).await;
        }
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
        let mut cancelled = false;
        let result = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
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
            ) => result,
        };
        if result.is_err()
            && !cancelled
            && let Some(known_delta) = request.known_delta.as_ref()
        {
            known_delta_store::record_execution(
                context.turn.config.codex_home.as_path(),
                known_delta,
                KnownDeltaExecutionObservation::CompleteFailure,
            )
            .await;
        }
        if cancelled && registration.committed {
            let terminated_processes = self
                .terminate_unpublished_processes_for_call_ids(std::slice::from_ref(
                    &context.call_id,
                ))
                .await
                .map_err(|error| {
                    UnifiedExecError::process_failed(format!(
                        "unified exec cancellation cleanup failed: {error}"
                    ))
                })?;
            if terminated_processes > 0 {
                mark_exec_process_exited();
            }
        } else if !registration.committed {
            let process_was_attached = registration.primary_process.is_some();
            if let Err(error) = registration.cleanup().await {
                return Err(UnifiedExecError::process_failed(format!(
                    "unified exec startup cleanup failed: {error}"
                )));
            }
            if cancelled && process_was_attached {
                mark_exec_process_exited();
            }
        }
        result
    }

    async fn exec_command_inner(
        &self,
        request: &mut ExecCommandRequest,
        process_id_reservation: &mut ProcessIdReservation,
        context: &UnifiedExecContext,
        registration: &mut PendingProcessRegistration,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let cwd = request.cwd.clone();
        let known_delta_executor_started_at = Instant::now();
        let executor_readiness_timing_guard = context
            .turn
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::ExecutorReadinessWait);
        let launch = self
            .open_session_with_sandbox(request, cwd.clone(), context, registration.pending_spawns())
            .await;
        drop(executor_readiness_timing_guard);

        let (launch, mut deferred_network_approval) = match launch {
            Ok((launch, deferred_network_approval)) => (launch, deferred_network_approval),
            Err(err) => {
                self.release_process_id(request.process_id).await;
                return Err(err);
            }
        };
        let process = match launch {
            UnifiedExecLaunch::Process(process) => process,
            UnifiedExecLaunch::KnownDelta(hit) => {
                let started_at = Instant::now();
                let event_ctx = ToolEventCtx::new(
                    context.session.as_ref(),
                    context.turn.as_ref(),
                    &context.call_id,
                    context.tracker.as_ref(),
                );
                let emitter = ToolEmitter::unified_exec(
                    &request.command_for_safety,
                    cwd.clone(),
                    ExecCommandSource::UnifiedExecStartup,
                    None,
                    request.turn_environment.environment_id.clone(),
                );
                emitter.emit(event_ctx, ToolEventStage::Begin).await;
                if let Err(message) = finish_deferred_network_approval_for_session(
                    Some(&context.session),
                    deferred_network_approval.take(),
                )
                .await
                {
                    self.release_process_id(request.process_id).await;
                    return Err(UnifiedExecError::process_failed(message));
                }
                let raw_output = hit.rendered_output().as_bytes().to_vec();
                let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
                transcript.lock().await.push_chunk(raw_output.clone());
                let wall_time = Instant::now().saturating_duration_since(started_at);
                emit_exec_end_for_unified_exec(
                    Arc::clone(&context.session),
                    Arc::clone(&context.turn),
                    context.call_id.clone(),
                    request.command_for_safety.clone(),
                    cwd,
                    request.turn_environment.environment_id.clone(),
                    None,
                    transcript,
                    String::new(),
                    None,
                    0,
                    false,
                    wall_time,
                    context.tracker.clone(),
                )
                .await;
                self.release_process_id(request.process_id).await;
                return Ok(ExecCommandToolOutput {
                    event_call_id: context.call_id.clone(),
                    chunk_id: generate_chunk_id(),
                    wall_time,
                    raw_output,
                    truncation_policy: context.turn.model_info.truncation_policy.into(),
                    max_output_tokens: request.max_output_tokens,
                    process_id: None,
                    exit_code: Some(0),
                    process_exited: true,
                    original_token_count: Some(approx_token_count(hit.rendered_output())),
                    hook_command: Some(request.hook_command.clone()),
                    raw_output_artifact: Some(hit.raw_output_artifact().clone()),
                    repair_notice: None,
                });
            }
        };
        registration.attach_process(Arc::clone(&process), deferred_network_approval.clone());
        let executor_was_ready = self.mark_executor_ready(&request.turn_environment.environment_id);
        let tool_execution_timing_guard = context.turn.turn_timing_state.begin_tool_execution();
        if let Some(deferred) = deferred_network_approval.as_ref() {
            terminate_process_on_network_denial(
                Arc::clone(&process),
                Arc::downgrade(&context.session),
                deferred.clone(),
            );
        }

        let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
        let event_ctx = ToolEventCtx::new(
            context.session.as_ref(),
            context.turn.as_ref(),
            &context.call_id,
            context.tracker.as_ref(),
        );
        let emitter = ToolEmitter::unified_exec(
            &request.command_for_safety,
            cwd.clone(),
            ExecCommandSource::UnifiedExecStartup,
            Some(request.process_id.to_string()),
            request.turn_environment.environment_id.clone(),
        );
        emitter.emit(event_ctx, ToolEventStage::Begin).await;

        let start = Instant::now();
        start_streaming_output(&process, context, Arc::clone(&transcript))?;
        // Persist live sessions before the initial yield wait so handler cancellation cannot
        // orphan the process. Mutating sessions are explicitly terminated when their owning turn
        // reaches a terminal state; non-mutating sessions remain resumable across turns.
        let process_started_alive = !process.has_exited() && process.exit_code().is_none();
        let _initial_exec_command_guard = if process_started_alive {
            let initial_exec_command_active = Arc::new(AtomicBool::new(true));
            registration.set_initial_exec_command_active(Arc::clone(&initial_exec_command_active));
            let store_result = self
                .store_process(
                    Arc::clone(&process),
                    context,
                    &request.command_for_safety,
                    request.hook_command.clone(),
                    cwd.clone(),
                    request.turn_environment.environment_id.clone(),
                    start,
                    request.process_id,
                    process_id_reservation,
                    request.tty,
                    request.attempt_key.clone(),
                    request.raw_output_artifact.clone(),
                    deferred_network_approval.clone(),
                    Arc::clone(&transcript),
                    Arc::clone(&initial_exec_command_active),
                    registration,
                    request.known_delta.clone(),
                    request
                        .known_delta
                        .as_ref()
                        .map(|_| known_delta_executor_started_at),
                )
                .await;
            store_result?;
            request.known_delta = None;
            Some(InitialExecCommandGuard {
                active: initial_exec_command_active,
            })
        } else {
            None
        };

        let yield_time_ms =
            clamp_yield_time_for_readiness(request.yield_time_ms, executor_was_ready);
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
        let deadline = start + Duration::from_millis(yield_time_ms);
        let collected = Self::collect_initial_output_until_deadline(
            &output_buffer,
            &output_notify,
            &output_closed,
            &output_closed_notify,
            &cancellation_token,
            Some(context.session.subscribe_elicitation_pause_state()),
            deadline,
        )
        .await;
        if cancellation_token.is_cancelled() || process.has_exited() {
            mark_exec_process_exited();
        }
        if !process_started_alive {
            let output_drain_started_at = Instant::now();
            wait_for_process_output_drain(&process.output_drained_token()).await;
            if let Some(timing) = active_tool_dispatch_timing() {
                timing.record_timer_wait(ToolLifecycleTimerWait {
                    wait_kind: "initial_process_output_drain".to_string(),
                    effective_timeout_ms: Some(
                        u64::try_from(output_drain_started_at.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                    ),
                    wake_reason: ToolLifecycleWakeReason::Completed,
                    ..Default::default()
                });
            }
        }
        drop(tool_execution_timing_guard);
        let wall_time = Instant::now().saturating_duration_since(start);

        let text = String::from_utf8_lossy(&collected).to_string();
        let chunk_id = generate_chunk_id();
        if deferred_network_approval
            .as_ref()
            .is_some_and(DeferredNetworkApproval::is_cancelled)
        {
            let message = network_denial_message_for_session(
                Some(&context.session),
                deferred_network_approval.take(),
            )
            .await;
            emit_failed_initial_exec_end_if_unstored(
                process_started_alive,
                Some(&process),
                context,
                request,
                cwd.clone(),
                Arc::clone(&transcript),
                text.clone(),
                message.clone(),
                wall_time,
            )
            .await;
            return Err(self
                .fail_process_with_message(request.process_id, &process, message)
                .await);
        }
        if let Some(message) = process.failure_message() {
            let finish_result = finish_deferred_network_approval_for_session(
                Some(&context.session),
                deferred_network_approval.take(),
            )
            .await;
            emit_failed_initial_exec_end_if_unstored(
                process_started_alive,
                Some(&process),
                context,
                request,
                cwd.clone(),
                Arc::clone(&transcript),
                text.clone(),
                message.clone(),
                wall_time,
            )
            .await;
            if let Err(message) = finish_result {
                return Err(self
                    .fail_process_with_message(request.process_id, &process, message)
                    .await);
            }
            return Err(self
                .fail_process_with_message(request.process_id, &process, message)
                .await);
        }
        let process_id = request.process_id;
        let (response_process_id, exit_code, process_exited) = if process_started_alive {
            match self.refresh_process_state(process_id).await {
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
                ProcessStatus::Exited { exit_code, entry } => {
                    if let Err(message) =
                        finish_deferred_network_approval_after_process_exit_for_session(
                            Some(&context.session),
                            deferred_network_approval.take(),
                        )
                        .await
                    {
                        return Err(self
                            .fail_process_with_message(process_id, &entry.process, message)
                            .await);
                    }
                    process.check_for_sandbox_denial_with_text(&text).await?;
                    (None, exit_code, true)
                }
                ProcessStatus::Unknown => {
                    return Err(UnifiedExecError::UnknownProcessId { process_id });
                }
            }
        } else {
            // Short-lived command: emit the completed command item immediately
            // using the same helper as the background watcher.
            let finish_result = finish_deferred_network_approval_after_process_exit_for_session(
                Some(&context.session),
                deferred_network_approval.take(),
            )
            .await;
            if let Err(message) = finish_result {
                emit_failed_initial_exec_end_if_unstored(
                    process_started_alive,
                    Some(&process),
                    context,
                    request,
                    cwd.clone(),
                    Arc::clone(&transcript),
                    text.clone(),
                    message.clone(),
                    wall_time,
                )
                .await;
                return Err(self
                    .fail_process_with_message(request.process_id, &process, message)
                    .await);
            }
            let exit_code = process.exit_code();
            let exit = exit_code.unwrap_or(-1);
            emit_exec_end_for_unified_exec(
                Arc::clone(&context.session),
                Arc::clone(&context.turn),
                context.call_id.clone(),
                request.command_for_safety.clone(),
                cwd.clone(),
                request.turn_environment.environment_id.clone(),
                Some(process_id.to_string()),
                Arc::clone(&transcript),
                text.clone(),
                Some(process.snapshot_completion_output().await),
                exit,
                false,
                wall_time,
                context.tracker.clone(),
            )
            .await;

            self.release_process_id(request.process_id).await;
            process.check_for_sandbox_denial_with_text(&text).await?;
            (None, exit_code, true)
        };

        let original_token_count = approx_token_count(&text);
        let response = ExecCommandToolOutput {
            event_call_id: context.call_id.clone(),
            chunk_id,
            wall_time,
            raw_output: collected,
            truncation_policy: context.turn.model_info.truncation_policy.into(),
            max_output_tokens: request.max_output_tokens,
            process_id: response_process_id,
            exit_code,
            process_exited,
            original_token_count: Some(original_token_count),
            hook_command: Some(request.hook_command.clone()),
            raw_output_artifact: process.raw_output_artifact().await,
            repair_notice: None,
        };

        if process_started_alive
            && let Some(finalized_artifact) = response.raw_output_artifact.clone()
        {
            context
                .session
                .services
                .command_execution
                .update_running_artifact(process_id, finalized_artifact)
                .await;
        }

        if response.process_id.is_none()
            && let Some(known_delta) = request.known_delta.as_ref()
        {
            let completion_output = process.snapshot_completion_output().await;
            record_known_delta_from_process_output(
                context.turn.config.codex_home.as_path(),
                known_delta,
                &completion_output,
                response.exit_code == Some(0) && !process.termination_was_requested(),
                Instant::now().saturating_duration_since(known_delta_executor_started_at),
            )
            .await;
        }

        Ok(response)
    }

    pub(crate) async fn write_stdin(
        &self,
        request: WriteStdinRequest<'_>,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let process_id = request.process_id;

        // Different terminal sessions can be polled concurrently, but reads and
        // writes against one terminal must not overlap because they share a
        // draining output buffer and process lifecycle.
        let locked_process = {
            let store = self.process_store.lock().await;
            let entry = store
                .processes
                .get(&process_id)
                .ok_or(UnifiedExecError::UnknownProcessId { process_id })?;
            Arc::clone(&entry.process)
        };
        let _interaction_guard = locked_process.interaction_lock().lock_owned().await;

        let PreparedProcessHandles {
            process,
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
            pause_state,
            session,
            network_approval,
            call_id,
            hook_command,
            process_id,
            tty,
            ..
        } = self
            .prepare_process_handles(process_id, &locked_process)
            .await?;
        let mut status_after_write = None;
        let yield_time_ms = {
            // Empty polls use configurable background timeout bounds. Non-empty
            // writes keep a fixed max cap so interactive stdin remains responsive.
            let time_ms = request.yield_time_ms.max(MIN_YIELD_TIME_MS);
            if request.input.is_empty() {
                time_ms.clamp(MIN_EMPTY_YIELD_TIME_MS, self.max_write_stdin_yield_time_ms)
            } else {
                time_ms.min(MAX_YIELD_TIME_MS)
            }
        };
        // The public yield timeout covers the entire interaction, including the
        // write and the short process-reaction window below.
        let start = Instant::now();
        let deadline = start + Duration::from_millis(yield_time_ms);

        if !request.input.is_empty() {
            if !tty {
                if request.input == INTERRUPT {
                    process.interrupt().await?;
                } else {
                    return Err(UnifiedExecError::StdinClosed);
                }
            } else {
                match process.write(request.input.as_bytes()).await {
                    Ok(()) => {}
                    Err(err) => {
                        let status = self.refresh_process_state(process_id).await;
                        if matches!(status, ProcessStatus::Exited { .. }) {
                            status_after_write = Some(status);
                        } else if let UnifiedExecError::ProcessFailed { message } = err {
                            return Err(self
                                .fail_process_with_message(process_id, &process, message)
                                .await);
                        } else {
                            return Err(err);
                        }
                    }
                }
            }
        }

        let collected = if request.input.is_empty() {
            // Empty stdin is an owner wait, not a fixed-cadence poll. Hold one
            // event-driven observation until meaningful output, exit, or the
            // owner deadline instead of waking every five seconds.
            Self::collect_output_until_progress_or_deadline(
                &output_buffer,
                &output_notify,
                &output_closed,
                &output_closed_notify,
                &cancellation_token,
                pause_state,
                deadline,
            )
            .await
        } else {
            Self::collect_output_until_deadline(
                &output_buffer,
                &output_notify,
                &output_closed,
                &output_closed_notify,
                &cancellation_token,
                pause_state,
                deadline,
            )
            .await
        };
        let wall_time = Instant::now().saturating_duration_since(start);

        let chunk_id = generate_chunk_id();
        if network_approval
            .as_ref()
            .is_some_and(DeferredNetworkApproval::is_cancelled)
        {
            let message =
                network_denial_message_for_session(session.as_ref(), network_approval.clone())
                    .await;
            return Err(self
                .fail_process_with_message(process_id, &process, message)
                .await);
        }
        if let Some(message) = process.failure_message() {
            let finish_result = finish_deferred_network_approval_for_session(
                session.as_ref(),
                network_approval.clone(),
            )
            .await;
            if let Err(message) = finish_result {
                return Err(self
                    .fail_process_with_message(process_id, &process, message)
                    .await);
            }
            return Err(self
                .fail_process_with_message(process_id, &process, message)
                .await);
        }

        // After polling, refresh_process_state tells us whether the PTY is
        // still alive or has exited and been removed from the store; we thread
        // that through so the handler can tag or suppress TerminalInteraction
        // with an appropriate process_id and exit_code.
        let status = if let Some(status) = status_after_write {
            status
        } else {
            self.refresh_process_state(process_id).await
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
                if let Err(message) =
                    finish_network_approval_after_process_exit_for_entry(&entry).await
                {
                    return Err(self
                        .fail_process_with_message(request.process_id, &entry.process, message)
                        .await);
                }
                (None, exit_code, true, call_id)
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
        let text = String::from_utf8_lossy(&collected).to_string();
        let original_token_count = approx_token_count(&text);

        let response = ExecCommandToolOutput {
            event_call_id,
            chunk_id,
            wall_time,
            raw_output: collected,
            truncation_policy: request.truncation_policy,
            max_output_tokens: request.max_output_tokens,
            process_id,
            exit_code,
            process_exited,
            original_token_count: Some(original_token_count),
            hook_command: Some(hook_command),
            raw_output_artifact: process.raw_output_artifact().await,
            repair_notice: None,
        };

        Ok(response)
    }

    async fn refresh_process_state(&self, process_id: u32) -> ProcessStatus {
        let mut store = self.process_store.lock().await;
        let Some(entry) = store.processes.get_mut(&process_id) else {
            return ProcessStatus::Unknown;
        };

        let exit_code = entry.process.exit_code();
        let process_id = entry.process_id;

        if entry.process.has_exited() && entry.process.output_is_closed() {
            let Some(entry) = store.remove(process_id) else {
                return ProcessStatus::Unknown;
            };
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
        let pause_state = entry
            .session
            .upgrade()
            .map(|session| session.subscribe_elicitation_pause_state());
        let session = entry.session.upgrade();

        Ok(PreparedProcessHandles {
            process: Arc::clone(&entry.process),
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
            pause_state,
            session,
            network_approval: entry.network_approval.clone(),
            call_id: entry.call_id.clone(),
            hook_command: entry.hook_command.clone(),
            process_id: entry.process_id,
            tty: entry.tty,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_process(
        &self,
        process: Arc<UnifiedExecProcess>,
        context: &UnifiedExecContext,
        command: &[String],
        hook_command: String,
        cwd: PathUri,
        environment_id: String,
        started_at: Instant,
        process_id: u32,
        process_id_reservation: &mut ProcessIdReservation,
        tty: bool,
        attempt_key: crate::tools::command_execution::CommandAttemptKey,
        raw_output_artifact: crate::tools::command_output_artifact::RawOutputArtifact,
        network_approval: Option<DeferredNetworkApproval>,
        transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
        initial_exec_command_active: Arc<AtomicBool>,
        registration: &mut PendingProcessRegistration,
        known_delta: Option<crate::tools::known_delta_store::PreparedKnownDelta>,
        known_delta_executor_started_at: Option<Instant>,
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
            hook_command,
            tty,
            network_approval,
            session: Arc::downgrade(&context.session),
            last_used: started_at,
        };
        let (pruned_entry, stored) = {
            let mut store = self.process_store.lock().await;
            let pruned_entry = Self::prune_processes_if_needed(&mut store);
            let stored = store.processes.len() < MAX_UNIFIED_EXEC_PROCESSES;
            if stored {
                store.processes.insert(process_id, entry);
                process_id_reservation.transfer_to_store();
            }
            (pruned_entry, stored)
        };
        // prune_processes_if_needed runs while holding process_store; do async
        // network-approval cleanup only after dropping that lock.
        if let Some(pruned_entry) = pruned_entry {
            unregister_network_approval_for_entry(&pruned_entry).await;
            let exit_code = pruned_entry.process.exit_code().unwrap_or(-1);
            context
                .session
                .services
                .command_execution
                .finish_running_process_with_execution_id(
                    pruned_entry.process_id,
                    pruned_entry.command_execution_id,
                    &pruned_entry.parent_tool_execution_id,
                    Some(exit_code),
                )
                .await;
            debug_assert!(pruned_entry.process.has_exited());
        }

        if !stored {
            let _ = process.terminate_confirmed().await;
            return Err(UnifiedExecError::process_failed(format!(
                "unified exec process limit ({MAX_UNIFIED_EXEC_PROCESSES}) reached; all slots are still active"
            )));
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
            )
            .await
        {
            let termination_error = process.terminate_confirmed().await.err();
            self.release_process_id(process_id).await;
            let message = match termination_error {
                Some(termination_error) => format!(
                    "{error}; additionally failed to terminate the untracked process: {termination_error}"
                ),
                None => error,
            };
            return Err(UnifiedExecError::process_failed(message));
        }

        spawn_exit_watcher(
            Arc::clone(&process),
            Arc::clone(&context.session),
            Arc::clone(&context.turn),
            context.call_id.clone(),
            command.to_vec(),
            cwd,
            environment_id,
            process_id,
            command_execution_id,
            parent_tool_execution_id,
            transcript,
            started_at,
            context.tracker.clone(),
            known_delta,
            known_delta_executor_started_at,
            tool_dispatch_timing,
        );
        registration.commit();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open_session_with_exec_env(
        &self,
        process_id: u32,
        command: SandboxCommand,
        additional_read_roots: Vec<AbsolutePathBuf>,
        options: ExecOptions,
        additional_permissions_uri: Option<
            &codex_protocol::request_permissions::UriAdditionalPermissionProfile,
        >,
        attempt: &SandboxAttempt<'_>,
        network: Option<&NetworkProxy>,
        environment_id: Option<&str>,
        exec_server_env_config: Option<ExecServerEnvConfig>,
        tty: bool,
        spawn_lifecycle: SpawnLifecycleHandle,
        raw_output_artifact: Option<crate::tools::command_output_artifact::RawOutputArtifact>,
        environment: &codex_exec_server::Environment,
        pending_spawns: &PendingSpawnRegistration,
    ) -> Result<Arc<UnifiedExecProcess>, ToolError> {
        let mut request = if environment.is_remote() {
            attempt.env_for_exec_server(
                command,
                options,
                network,
                environment_id,
                additional_permissions_uri,
            )
        } else {
            attempt.env_for(command, options, network, environment_id)
        }
        .map_err(ToolError::Codex)?;
        request.windows_sandbox_additional_read_roots = additional_read_roots;
        request.exec_server_env_config = exec_server_env_config;
        self.open_session_with_prepared_exec_env(
            process_id,
            &request,
            tty,
            spawn_lifecycle,
            raw_output_artifact,
            environment,
            pending_spawns,
        )
        .await
        .map_err(|err| match err {
            UnifiedExecError::SandboxDenied { output, .. } => {
                ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                    output: Box::new(output),
                    network_policy_decision: None,
                }))
            }
            other => ToolError::Rejected(other.to_string()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open_session_with_prepared_exec_env(
        &self,
        process_id: u32,
        request: &ExecRequest,
        tty: bool,
        mut spawn_lifecycle: SpawnLifecycleHandle,
        raw_output_artifact: Option<crate::tools::command_output_artifact::RawOutputArtifact>,
        environment: &codex_exec_server::Environment,
        pending_spawns: &PendingSpawnRegistration,
    ) -> Result<Arc<UnifiedExecProcess>, UnifiedExecError> {
        let inherited_fds = spawn_lifecycle.inherited_fds();

        if request.sandbox == codex_sandboxing::SandboxType::WindowsRestrictedToken {
            // TODO(anp): Keep PathUri through the Windows sandbox launch boundary.
            let native_cwd =
                request
                    .cwd
                    .to_abs_path()
                    .map_err(|_| UnifiedExecError::ForeignPath {
                        path: request.cwd.clone(),
                    })?;
            let additional_deny_write_paths = request
                .windows_sandbox_filesystem_overrides
                .as_ref()
                .map(|overrides| overrides.additional_deny_write_paths.clone())
                .unwrap_or_default();
            let additional_deny_read_paths = request
                .windows_sandbox_filesystem_overrides
                .as_ref()
                .map(|overrides| overrides.additional_deny_read_paths.clone())
                .unwrap_or_default();
            let elevated_read_roots_override = request
                .windows_sandbox_filesystem_overrides
                .as_ref()
                .and_then(|overrides| overrides.read_roots_override.clone());
            let elevated_read_roots_include_platform_defaults = request
                .windows_sandbox_filesystem_overrides
                .as_ref()
                .is_some_and(|overrides| overrides.read_roots_include_platform_defaults);
            let elevated_write_roots_override = request
                .windows_sandbox_filesystem_overrides
                .as_ref()
                .and_then(|overrides| overrides.write_roots_override.clone());
            let spawned = match request.windows_sandbox_level {
                codex_protocol::config_types::WindowsSandboxLevel::Elevated => {
                    codex_windows_sandbox::spawn_windows_sandbox_session_elevated_for_permission_profile(
                        &request.permission_profile,
                        request.windows_sandbox_workspace_roots.as_slice(),
                        request.codex_home.as_path(),
                        request.command.clone(),
                        native_cwd.as_path(),
                        request.env.clone(),
                        request.network.is_some(),
                        None,
                        elevated_read_roots_override.as_deref(),
                        &request.windows_sandbox_additional_read_roots,
                        elevated_read_roots_include_platform_defaults,
                        elevated_write_roots_override.as_deref(),
                        &additional_deny_read_paths,
                        &additional_deny_write_paths,
                        tty,
                        tty,
                        request.windows_sandbox_private_desktop,
                    )
                    .await
                }
                codex_protocol::config_types::WindowsSandboxLevel::RestrictedToken
                | codex_protocol::config_types::WindowsSandboxLevel::Disabled => {
                    codex_windows_sandbox::spawn_windows_sandbox_session_legacy(
                        &request.permission_profile,
                        request.windows_sandbox_workspace_roots.as_slice(),
                        request.codex_home.as_path(),
                        request.command.clone(),
                        native_cwd.as_path(),
                        request.env.clone(),
                        None,
                        &additional_deny_read_paths,
                        &additional_deny_write_paths,
                        tty,
                        tty,
                        request.windows_sandbox_private_desktop,
                    )
                    .await
                }
            };
            let spawned =
                spawned.map_err(|err| UnifiedExecError::create_process(err.to_string()))?;
            spawn_lifecycle.after_spawn();
            mark_exec_process_spawned();
            return UnifiedExecProcess::from_spawned(
                spawned,
                request.sandbox,
                spawn_lifecycle,
                raw_output_artifact,
                pending_spawns,
            )
            .await;
        }
        if environment.is_remote() {
            if !inherited_fds.is_empty() {
                return Err(UnifiedExecError::create_process(
                    "remote exec-server does not support inherited file descriptors".to_string(),
                ));
            }

            let started = environment
                .get_exec_backend()
                .start(exec_server_params_for_request(process_id, request, tty))
                .await
                .map_err(|err| UnifiedExecError::create_process(err.to_string()))?;
            spawn_lifecycle.after_spawn();
            mark_exec_process_spawned();
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
        let spawn_result = if tty {
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
        let spawned =
            spawn_result.map_err(|err| UnifiedExecError::create_process(err.to_string()))?;
        spawn_lifecycle.after_spawn();
        mark_exec_process_spawned();
        UnifiedExecProcess::from_spawned(
            spawned,
            request.sandbox,
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
    ) -> Result<(UnifiedExecLaunch, Option<DeferredNetworkApproval>), UnifiedExecError> {
        let (env, local_policy_env) = build_unified_exec_environment(context);
        let exec_server_env_config = ExecServerEnvConfig {
            policy: exec_env_policy_from_shell_policy(
                &context.turn.config.permissions.shell_environment_policy,
            ),
            local_policy_env,
        };
        let mut orchestrator = ToolOrchestrator::new();
        let mut runtime = UnifiedExecRuntime::new_with_pending_spawns(self, pending_spawns);

        let proven_direct_argv = if request.shell_wrapper_is_owned
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
        };

        let canonical_exec_approval_requirement = if let Some(proof) = proven_direct_argv.as_ref() {
            Some(
                context
                    .session
                    .services
                    .exec_policy
                    .create_exec_approval_requirement_for_command(ExecApprovalRequest {
                        command: proof.command_for_policy(),
                        command_for_safety: None,
                        approval_policy: context.turn.approval_policy.value(),
                        permission_profile: context.turn.permission_profile(),
                        windows_sandbox_level: context.turn.windows_sandbox_level,
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
        let exec_approval_request = ExecApprovalRequest {
            command: &request.command,
            command_for_safety: Some(&request.command_for_safety),
            approval_policy: context.turn.approval_policy.value(),
            permission_profile: context.turn.permission_profile(),
            windows_sandbox_level: context.turn.windows_sandbox_level,
            sandbox_permissions: if request.additional_permissions_preapproved {
                crate::sandboxing::SandboxPermissions::UseDefault
            } else {
                request.sandbox_permissions
            },
            prefix_rule: request.prefix_rule.clone(),
        };
        let exec_approval_requirement = if request.shell_wrapper_is_owned {
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
        };

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

            normalization_cwd: request.normalization_cwd.clone(),

            approved_powershell_direct_argv,
            raw_output_artifact: request.raw_output_artifact.clone(),
            shell_type: request.shell_type,
            hook_command: request.hook_command.clone(),
            process_id: request.process_id,
            cwd,
            sandbox_cwd: request.sandbox_cwd.clone(),
            turn_environment: request.turn_environment.clone(),
            env,
            exec_server_env_config: Some(exec_server_env_config),
            explicit_env_overrides: context
                .turn
                .config
                .permissions
                .shell_environment_policy
                .r#set
                .clone(),
            network: request.network.clone(),
            tty: request.tty,
            sandbox_permissions: request.sandbox_permissions,
            additional_permissions: request.additional_permissions.clone(),
            additional_permissions_uri: request.additional_permissions_uri.clone(),
            justification: request.justification.clone(),
            exec_approval_requirement,
            validation_launch: request.validation_launch.clone(),
            known_delta_hit: request
                .known_delta
                .as_ref()
                .and_then(|prepared| prepared.hit().cloned()),
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
            .map(|result| (result.output, result.deferred_network_approval))
            .map_err(|err| match err {
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
                ToolError::ValidationSkipped(skipped) => {
                    UnifiedExecError::ValidationSkipped(skipped)
                }
                other => UnifiedExecError::create_process(format!("{other:?}")),
            })
    }

    pub(super) async fn collect_output_until_deadline(
        output_buffer: &OutputBuffer,
        output_notify: &Arc<Notify>,
        output_closed: &Arc<AtomicBool>,
        output_closed_notify: &Arc<Notify>,
        cancellation_token: &CancellationToken,
        pause_state: Option<watch::Receiver<bool>>,
        deadline: Instant,
    ) -> Vec<u8> {
        Self::collect_output_until_deadline_with_quiet_yield(
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
            pause_state,
            deadline,
            None,
            false,
        )
        .await
    }

    async fn collect_output_until_progress_or_deadline(
        output_buffer: &OutputBuffer,
        output_notify: &Arc<Notify>,
        output_closed: &Arc<AtomicBool>,
        output_closed_notify: &Arc<Notify>,
        cancellation_token: &CancellationToken,
        pause_state: Option<watch::Receiver<bool>>,
        deadline: Instant,
    ) -> Vec<u8> {
        Self::collect_output_until_deadline_with_quiet_yield(
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
            pause_state,
            deadline,
            Some(INITIAL_OUTPUT_QUIET_PERIOD),
            false,
        )
        .await
    }

    async fn collect_initial_output_until_deadline(
        output_buffer: &OutputBuffer,
        output_notify: &Arc<Notify>,
        output_closed: &Arc<AtomicBool>,
        output_closed_notify: &Arc<Notify>,
        cancellation_token: &CancellationToken,
        pause_state: Option<watch::Receiver<bool>>,
        deadline: Instant,
    ) -> Vec<u8> {
        Self::collect_output_until_deadline_with_quiet_yield(
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
            pause_state,
            deadline,
            Some(INITIAL_OUTPUT_QUIET_PERIOD),
            true,
        )
        .await
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
        quiet_period: Option<Duration>,
        yield_when_silent: bool,
    ) -> Vec<u8> {
        let mut collected: Vec<u8> = Vec::with_capacity(4096);
        let mut lagged_chunks = 0_u64;
        let tool_dispatch_timing = active_tool_dispatch_timing();
        let mut wait_attempt = 0_u32;
        let mut exit_signal_received = cancellation_token.is_cancelled();
        let mut post_exit_deadline: Option<Instant> = None;
        // A silent initial process should return a live session promptly instead
        // of consuming the full requested yield. Meaningful output resets this
        // adaptive deadline so a burst can still be collected to quiescence.
        let mut early_yield_deadline = yield_when_silent
            .then(|| quiet_period.map(|period| (Instant::now() + period).min(deadline)))
            .flatten();
        loop {
            Self::extend_deadlines_while_paused(
                &mut pause_state,
                &mut deadline,
                &mut post_exit_deadline,
                &mut early_yield_deadline,
            )
            .await;
            // Register before inspecting the buffer. `notify_waiters` does not
            // retain a permit, so registering after an empty drain would leave
            // a race where output can arrive between the drain and the first
            // poll of `notified()`, stranding an owner-held wait until its
            // deadline.
            let output_notified = output_notify.notified();
            tokio::pin!(output_notified);
            output_notified.as_mut().enable();
            let drained_chunks: Vec<Vec<u8>>;
            let drained_omitted_bytes: usize;
            let drained_lagged_chunks: u64;
            {
                let mut guard = output_buffer.lock().await;
                drained_omitted_bytes = guard.take_unreported_omitted_bytes();
                let omission_marker = (drained_omitted_bytes > 0)
                    .then(|| omitted_output_marker(drained_omitted_bytes));
                drained_chunks = guard.drain_chunks_with_omission_marker(omission_marker);
                drained_lagged_chunks = guard.take_unreported_lagged_chunks();
            }
            lagged_chunks = lagged_chunks.saturating_add(drained_lagged_chunks);

            if drained_chunks.is_empty() && drained_omitted_bytes == 0 && drained_lagged_chunks == 0
            {
                let closed = output_closed_notify.notified();
                tokio::pin!(closed);
                closed.as_mut().enable();
                exit_signal_received |= cancellation_token.is_cancelled();
                if exit_signal_received && output_closed.load(std::sync::atomic::Ordering::Acquire)
                {
                    break;
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
                    break;
                }

                if exit_signal_received {
                    let now = Instant::now();
                    let close_wait_deadline = *post_exit_deadline.get_or_insert_with(|| {
                        let period = quiet_period.unwrap_or(INITIAL_OUTPUT_QUIET_PERIOD);
                        (now + period).min(deadline)
                    });
                    let close_wait_remaining = close_wait_deadline.saturating_duration_since(now);
                    if close_wait_remaining == Duration::ZERO {
                        break;
                    }
                    if wait_attempt > 0
                        && let Some(timing) = tool_dispatch_timing.as_ref()
                    {
                        timing.increment_reentry_count();
                        timing.increment_retry_count();
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
                        turn_timing.record_next_sample_block_reason(
                            NextSampleBlockReason::WaitingForProcessCleanup,
                        );
                    }
                    let wake_reason = tokio::select! {
                        _ = &mut output_notified => ToolLifecycleWakeReason::Completed,
                        _ = &mut closed => ToolLifecycleWakeReason::Completed,
                        _ = tokio::time::sleep(close_wait_remaining) => ToolLifecycleWakeReason::Timeout,
                        _ = Self::wait_for_pause_change(pause_state.as_ref()) => ToolLifecycleWakeReason::Retry,
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
                        break;
                    }
                    continue;
                }

                let exit_notified = cancellation_token.cancelled();
                tokio::pin!(exit_notified);
                if wait_attempt > 0
                    && let Some(timing) = tool_dispatch_timing.as_ref()
                {
                    timing.increment_reentry_count();
                    timing.increment_retry_count();
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
                    turn_timing.record_next_sample_block_reason(
                        NextSampleBlockReason::WaitingForProcessCleanup,
                    );
                }
                let wake_reason = tokio::select! {
                    _ = &mut output_notified => ToolLifecycleWakeReason::Completed,
                    _ = &mut exit_notified => {
                        exit_signal_received = true;
                        ToolLifecycleWakeReason::Completed
                    },
                    _ = tokio::time::sleep(remaining) => ToolLifecycleWakeReason::Timeout,
                    _ = Self::wait_for_pause_change(pause_state.as_ref()) => ToolLifecycleWakeReason::Retry,
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
                    break;
                }
                continue;
            }

            let output_arrived = !drained_chunks.is_empty()
                || drained_omitted_bytes > 0
                || drained_lagged_chunks > 0;
            let meaningful_output_arrived = quiet_period.is_some()
                && (drained_omitted_bytes > 0
                    || drained_lagged_chunks > 0
                    || drained_chunks
                        .iter()
                        .any(|chunk| chunk.iter().any(|byte| !byte.is_ascii_whitespace())));
            for chunk in drained_chunks {
                collected.extend_from_slice(&chunk);
            }

            if meaningful_output_arrived {
                let quiet_deadline = Instant::now() + quiet_period.unwrap_or_default();
                early_yield_deadline = Some(quiet_deadline.min(deadline));
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
                break;
            }
        }

        if lagged_chunks > 0 {
            collected.extend_from_slice(&lagged_output_marker(lagged_chunks));
        }
        collected
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
        if !*receiver.borrow() {
            return;
        }

        let paused_at = Instant::now();
        while *receiver.borrow() {
            if receiver.changed().await.is_err() {
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

    async fn wait_for_pause_change(pause_state: Option<&watch::Receiver<bool>>) {
        match pause_state {
            Some(pause_state) => {
                let mut receiver = pause_state.clone();
                let _ = receiver.changed().await;
            }
            None => std::future::pending::<()>().await,
        }
    }

    fn prune_processes_if_needed(store: &mut ProcessStore) -> Option<ProcessEntry> {
        if store.processes.len() < MAX_UNIFIED_EXEC_PROCESSES {
            return None;
        }

        let mut meta: Vec<(u32, Instant, bool)> = store
            .processes
            .iter()
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
                return store.remove(process_id);
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

    pub(crate) async fn terminate_all_processes(&self) {
        let processes: Vec<(u32, Arc<UnifiedExecProcess>)> = {
            let mut processes = self.process_store.lock().await;
            let entries = processes
                .processes
                .iter()
                .map(|(process_id, entry)| (*process_id, Arc::clone(&entry.process)))
                .collect();
            processes.reserved_process_ids.clear();
            entries
        };

        let termination_results = futures::future::join_all(processes.into_iter().map(
            |(process_id, process)| async move {
                let result = process.terminate_confirmed().await;
                (process_id, process, result)
            },
        ))
        .await;

        for (process_id, process, result) in termination_results {
            if let Err(err) = result {
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
            if let Some(entry) = entry {
                unregister_network_approval_for_entry(&entry).await;
            }
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
                    (
                        *process_id,
                        entry.call_id.clone(),
                        Arc::clone(&entry.process),
                    )
                })
                .collect::<Vec<_>>()
        };
        targets.sort_by_key(|(process_id, _, _)| *process_id);

        let mut first_error = None;
        for (process_id, call_id, process) in &targets {
            if !process.has_exited()
                && let Err(error) = process.terminate_confirmed().await
                && first_error.is_none()
            {
                first_error = Some(format!(
                    "process {process_id} for unpublished call {call_id} did not confirm termination: {error}"
                ));
            }
        }
        if let Some(error) = first_error {
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
            unregister_network_approval_for_entry(entry).await;
            if let Some(session) = entry.session.upgrade() {
                session
                    .services
                    .command_execution
                    .finish_running_process_with_execution_id(
                        entry.process_id,
                        entry.command_execution_id,
                        &entry.parent_tool_execution_id,
                        Some(entry.process.exit_code().unwrap_or(-1)),
                    )
                    .await;
            }
        }

        Ok(removed_entries.len())
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
        let (process, already_exited) = {
            let store = self.process_store.lock().await;
            let Some(entry) = store.processes.get(&process_id) else {
                return false;
            };
            (Arc::clone(&entry.process), entry.process.has_exited())
        };

        if !already_exited && process.terminate_confirmed().await.is_err() {
            return false;
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

        unregister_network_approval_for_entry(&entry).await;
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

#[cfg(test)]
#[path = "process_manager_tests.rs"]
mod tests;
