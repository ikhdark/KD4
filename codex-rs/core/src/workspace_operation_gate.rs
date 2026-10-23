use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::ThreadId;
use codex_protocol::protocol::ToolLifecycleTimerWait;
use codex_protocol::protocol::ToolLifecycleWakeReason;
use tokio_util::sync::CancellationToken;

use crate::scoped_workspace_gate::NativeAdditions;
use crate::scoped_workspace_gate::ScopedWorkspaceGate;
use crate::scoped_workspace_gate::WorkspaceLease;

#[path = "workspace_validation_lease.rs"]
mod validation_lease;

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
    state: Mutex<ValidationGateState>,
    /// Woken whenever the gate may have become admissible: a permit dropped
    /// or a queued contender left. Contenders re-check `state` under its lock.
    released: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct ValidationGateState {
    holder: Option<ValidationHolder>,
    /// Tickets of contenders waiting for admission, in arrival order. Only the
    /// front contender may take a free gate, so later arrivals cannot starve it.
    queue: VecDeque<u64>,
    next_ticket: u64,
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
    _external: Option<validation_lease::ValidationLease>,
}

impl Drop for ValidationPermit {
    fn drop(&mut self) {
        // Release the host lease before waking a local successor. No local
        // gate mutex is held while its record is cleared.
        drop(self._external.take());
        // Holder state IS the admission lock: clear it under the same mutex
        // used for admission, before unlocking. No departing owner can erase
        // a successor, and no async guard or second lock order is needed.
        self.gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .holder
            .take();
        // A contender's `Notified` exists before it inspects the state, so a
        // release after that inspection still wakes it.
        self.gate.released.notify_waiters();
    }
}

/// A contender's place in its gate's admission queue, given up when its wait
/// ends for any reason, including cancellation by dropping the wait.
struct QueuedContender {
    gate: Arc<ValidationGate>,
    ticket: u64,
}

impl Drop for QueuedContender {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(index) = state.queue.iter().position(|ticket| *ticket == self.ticket) else {
            return;
        };
        state.queue.remove(index);
        drop(state);
        // A departing queue front may leave a free gate to its successor.
        self.gate.released.notify_waiters();
    }
}

/// Longest a launch queues in this process behind another session's
/// validation before the busy result is returned. In saved eight-session runs
/// a contended `cargo check` holder released a median 53 s (max 96 s) after
/// the refused contender arrived.
const VALIDATION_ADMISSION_WAIT_CAP: Duration = Duration::from_secs(120);

/// Admission ends this long before the caller's return bound, leaving time to
/// launch the admitted process and return its handle.
const VALIDATION_LAUNCH_RESERVE: Duration = Duration::from_secs(5);

/// Budget for queueing a launch behind another session's validation.
///
/// Tool dispatch scopes it around every call; a code-mode call's wrapper
/// deadline also bounds the wait. Unscoped callers keep the immediate busy
/// result.
#[derive(Clone, Debug)]
pub(crate) struct ValidationAdmissionWait {
    return_bound: Option<tokio::time::Instant>,
    cancellation: CancellationToken,
    turn_waiters: Option<Arc<TurnAdmissionWaiters>>,
}

impl ValidationAdmissionWait {
    /// `return_bound` is the latest instant the caller can still return a
    /// result. Cancelling `cancellation` ends a queued wait immediately.
    pub(crate) fn new(
        return_bound: Option<tokio::time::Instant>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            return_bound,
            cancellation,
            turn_waiters: None,
        }
    }

    /// A queued launch keeps its turn's workspace lease, so it stops queueing
    /// as soon as another call of that turn waits for workspace admission.
    pub(crate) fn yielding_to(mut self, turn_waiters: Arc<TurnAdmissionWaiters>) -> Self {
        self.turn_waiters = Some(turn_waiters);
        self
    }

    fn turn_is_waiting(&self) -> bool {
        self.turn_waiters
            .as_ref()
            .is_some_and(|waiters| waiters.waiting.load(Ordering::SeqCst) > 0)
    }

    async fn turn_waiter_arrived(&self) {
        let Some(waiters) = &self.turn_waiters else {
            return std::future::pending().await;
        };
        loop {
            // Register before checking, so an arrival after the check wakes it.
            let arrived = waiters.arrived.notified();
            if self.turn_is_waiting() {
                return;
            }
            arrived.await;
        }
    }

    fn deadline(&self, started: tokio::time::Instant) -> tokio::time::Instant {
        let capped = started + VALIDATION_ADMISSION_WAIT_CAP;
        self.return_bound.map_or(capped, |bound| {
            bound
                .checked_sub(VALIDATION_LAUNCH_RESERVE)
                .map_or(started, |bound| bound.max(started))
                .min(capped)
        })
    }
}

/// Calls of one turn that are blocked on that turn's workspace admission.
#[derive(Debug, Default)]
pub(crate) struct TurnAdmissionWaiters {
    waiting: AtomicUsize,
    arrived: tokio::sync::Notify,
}

impl TurnAdmissionWaiters {
    /// Runs one workspace admission, counted as waiting from its first
    /// pending poll until it completes or is dropped.
    pub(crate) async fn track<F: std::future::Future>(&self, admission: F) -> F::Output {
        let mut admission = std::pin::pin!(admission);
        let first = std::future::poll_fn(|cx| Poll::Ready(admission.as_mut().poll(cx))).await;
        if let Poll::Ready(output) = first {
            return output;
        }
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let _waiting = TurnAdmissionWaiter(self);
        self.arrived.notify_waiters();
        admission.await
    }
}

struct TurnAdmissionWaiter<'a>(&'a TurnAdmissionWaiters);

impl Drop for TurnAdmissionWaiter<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

tokio::task_local! {
    static VALIDATION_ADMISSION_WAIT: Option<ValidationAdmissionWait>;
}

/// Runs `future` with the budget its validation launches may queue for.
pub(crate) fn scope_validation_admission_wait<F: std::future::Future>(
    wait: Option<ValidationAdmissionWait>,
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    VALIDATION_ADMISSION_WAIT.scope(wait, future)
}

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
    // Only other patches retain this gate. Validation uses an independent lane
    // and its existing freshness check rejects results invalidated by a patch.
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

/// Admission before a process handle exists. Inside a scoped
/// [`ValidationAdmissionWait`], a launch queues in arrival order behind another
/// thread's holder until that budget ends; otherwise contention returns at
/// once. A refused launch leaves validation pending and starts no process.
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
        WorkspaceIdentity::Environment(environment.coordination_resource_id())
    };
    let wait = VALIDATION_ADMISSION_WAIT
        .try_with(Clone::clone)
        .ok()
        .flatten();
    acquire_operation(identity, command, owner, wait).await
}

/// Legacy shell validation runs to completion inside its call, so a queued
/// wait would consume that call's own deadline. Contention returns at once.
pub(crate) async fn acquire_workspace_validation(
    root: &Path,
    command: &str,
    owner: ValidationOwner,
) -> Result<ValidationPermit, String> {
    acquire_operation(
        local_identity(root).await,
        command,
        owner,
        /*wait*/ None,
    )
    .await
}

async fn local_identity(root: &Path) -> WorkspaceIdentity {
    let identity = tokio::fs::canonicalize(root)
        .await
        .map(|path| dunce::simplified(&path).to_path_buf())
        .unwrap_or_else(|_| root.to_path_buf());
    WorkspaceIdentity::Local(identity)
}

fn validation_gate(workspace: WorkspaceIdentity) -> Arc<ValidationGate> {
    let identity = WorkspaceIdentity::Validation(Box::new(workspace));
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
}

/// Admission that queued behind another holder joins the dispatch profile as a
/// `validation_gate` wait; a refusal that is not a cancellation is a timeout.
async fn acquire_operation(
    workspace: WorkspaceIdentity,
    command: &str,
    owner: ValidationOwner,
    wait: Option<ValidationAdmissionWait>,
) -> Result<ValidationPermit, String> {
    let started = tokio::time::Instant::now();
    let record = GateWaitRecord::begin(
        "validation_gate",
        wait.as_ref()
            .map(|wait| wait.deadline(started).saturating_duration_since(started)),
    );
    let cancellation = wait.as_ref().map(|wait| wait.cancellation.clone());
    let result = acquire_unrecorded_operation(workspace, command, owner, wait).await;
    record.finish(match &result {
        Ok(_) => ToolLifecycleWakeReason::Completed,
        Err(_)
            if cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled) =>
        {
            ToolLifecycleWakeReason::Cancelled
        }
        Err(_) => ToolLifecycleWakeReason::Timeout,
    });
    result
}

// The local FIFO gate is followed by a cross-process lease for host workspaces.
async fn acquire_unrecorded_operation(
    workspace: WorkspaceIdentity,
    command: &str,
    owner: ValidationOwner,
    wait: Option<ValidationAdmissionWait>,
) -> Result<ValidationPermit, String> {
    let started = tokio::time::Instant::now();
    let deadline = wait.as_ref().map(|wait| wait.deadline(started));
    let mut permit =
        acquire_local_operation(workspace.clone(), command, owner, wait.clone()).await?;
    let WorkspaceIdentity::Local(root) = workspace else {
        return Ok(permit);
    };
    loop {
        let (root, command) = (root.clone(), command.to_owned());
        let acquisition = tokio::task::spawn_blocking(move || {
            validation_lease::ValidationLease::try_acquire(&root, &command)
        });
        let acquired = if let (Some(wait), Some(deadline)) = (&wait, deadline) {
            tokio::select! {
                biased;
                () = wait.cancellation.cancelled() => return Err(cancelled_message(&permit.gate, started.elapsed())),
                () = tokio::time::sleep_until(deadline) => return Err("Validation busy: cross-process admission budget ended. This validation did not run; no process was started.".into()),
                result = acquisition => result,
            }
        } else {
            acquisition.await
        }.map_err(|error| format!("Validation admission worker failed: {error}. This validation did not run."))?
            .map_err(|error| format!("Validation admission failed: {error}"))?;
        if let Some(lease) = acquired {
            permit._external = Some(lease);
            return Ok(permit);
        }
        let (Some(wait), Some(deadline)) = (&wait, deadline) else {
            return Err("Validation busy: another KD4 process owns this workspace. This validation did not run. No process was started or queued; required validation remains pending.".into());
        };
        tokio::select! {
            biased;
            () = wait.cancellation.cancelled() => return Err(cancelled_message(&permit.gate, started.elapsed())),
            () = wait.turn_waiter_arrived() => return Err("Validation admission yielded to another call in this turn. This validation did not run; no process was started.".into()),
            () = tokio::time::sleep_until(deadline) => return Err("Validation busy: cross-process admission budget ended. This validation did not run; no process was started.".into()),
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
}

async fn acquire_local_operation(
    workspace: WorkspaceIdentity,
    command: &str,
    owner: ValidationOwner,
    wait: Option<ValidationAdmissionWait>,
) -> Result<ValidationPermit, String> {
    let gate = validation_gate(workspace);
    let started = tokio::time::Instant::now();
    let budget = wait.as_ref().map(|wait| (wait, wait.deadline(started)));
    let mut queued: Option<QueuedContender> = None;
    loop {
        // Create the wakeup before inspecting state: a release after the
        // inspection then completes it, so no wakeup can be lost.
        let released = gate.released.notified();
        tokio::pin!(released);
        let (wait, deadline) = {
            let mut state = gate
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ahead = match &queued {
                Some(queued) => state
                    .queue
                    .iter()
                    .position(|ticket| *ticket == queued.ticket)
                    .unwrap_or_default(),
                None => state.queue.len(),
            };
            if state.holder.is_none() && ahead == 0 {
                if queued.is_some() {
                    state.queue.pop_front();
                }
                state.holder = Some(ValidationHolder {
                    command: command.to_owned(),
                    admitted_at: Instant::now(),
                    owner: owner.thread_id,
                });
                drop(state);
                return Ok(ValidationPermit {
                    gate: Arc::clone(&gate),
                    _external: None,
                });
            }
            // Waiting cannot help this thread's own holder: resume it instead.
            let own_holder = state
                .holder
                .as_ref()
                .is_some_and(|holder| holder.owner == owner.thread_id);
            // A queued launch keeps its turn's workspace lease. Once another
            // call of the turn waits for admission, stop blocking that call.
            let yielded = budget.is_some_and(|(wait, _)| wait.turn_is_waiting());
            let Some((wait, deadline)) = budget.filter(|(_, deadline)| {
                !own_holder && !yielded && tokio::time::Instant::now() < *deadline
            }) else {
                let waited = queued.as_ref().map(|_| started.elapsed());
                let message =
                    busy_message(state.holder.as_ref(), ahead, own_holder, waited, yielded);
                drop(state);
                // Dropping `queued` gives up this contender's place in line.
                return Err(message);
            };
            if queued.is_none() {
                let ticket = state.next_ticket;
                state.next_ticket = ticket.wrapping_add(1);
                state.queue.push_back(ticket);
                queued = Some(QueuedContender {
                    gate: Arc::clone(&gate),
                    ticket,
                });
            }
            (wait, deadline)
        };
        tokio::select! {
            biased;
            () = wait.cancellation.cancelled() => {
                return Err(cancelled_message(&gate, started.elapsed()));
            }
            () = &mut released => {}
            () = wait.turn_waiter_arrived() => {}
            () = tokio::time::sleep_until(deadline) => {}
        }
    }
}

fn busy_message(
    holder: Option<&ValidationHolder>,
    ahead: usize,
    own_holder: bool,
    waited: Option<Duration>,
    yielded: bool,
) -> String {
    let mut message = match holder {
        Some(holder) => format!(
            "Validation busy: holder command {:?} (held for {:.3} seconds).",
            holder.command,
            holder.admitted_at.elapsed().as_secs_f64(),
        ),
        None => "Validation busy: the previous holder finished, and an earlier queued validation is admitted first.".to_owned(),
    };
    let ahead = match ahead {
        0 => String::new(),
        1 => " behind 1 earlier launch".to_owned(),
        ahead => format!(" behind {ahead} earlier launches"),
    };
    match waited {
        Some(waited) if yielded => message.push_str(&format!(
            " The harness queued this launch for {:.3} seconds{ahead}, then stopped because another call in this turn is waiting for workspace admission.",
            waited.as_secs_f64(),
        )),
        Some(waited) => message.push_str(&format!(
            " The harness queued this launch for {:.3} seconds{ahead}, the longest this call may wait.",
            waited.as_secs_f64(),
        )),
        None if yielded => message.push_str(
            " The harness did not queue this launch because another call in this turn is waiting for workspace admission.",
        ),
        None => {}
    }
    message.push_str(
        " This validation did not run. No process was started or queued; required validation remains pending. ",
    );
    if own_holder {
        message
            .push_str("Resume the existing operation rather than relaunching or switching lanes.");
        return message;
    }
    if holder.is_some() {
        message.push_str(
            "The holder belongs to another session or agent, so it cannot be resumed from here. ",
        );
    }
    message.push_str(if yielded {
        "Do not poll, sleep, or script a wait for it; once the concurrent calls finish, rerun this command once when you need its result, without concurrent calls in its cell, so the harness can queue it."
    } else if waited.is_some() {
        "Do not poll, sleep, or script a wait for it: the harness already queued this launch. Continue other work, then rerun this command once when you need its result."
    } else {
        "Do not poll, sleep, or script a wait for it; continue other work, then rerun this command once when you need its result."
    });
    message
}

fn cancelled_message(gate: &ValidationGate, waited: Duration) -> String {
    let state = gate
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let holder = state.holder.as_ref().map_or_else(String::new, |holder| {
        format!(" behind holder command {:?}", holder.command)
    });
    format!(
        "Validation admission was cancelled after queueing for {:.3} seconds{holder}. This validation did not run. No process was started or queued; required validation remains pending.",
        waited.as_secs_f64(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn audit151_cancelled_external_wait_releases_the_local_gate() {
        let repo = tempfile::tempdir().unwrap();
        let workspace = WorkspaceIdentity::Local(repo.path().to_path_buf());
        let external = validation_lease::ValidationLease::try_acquire(repo.path(), "other host")
            .unwrap()
            .unwrap();
        let cancellation = CancellationToken::new();
        let pending = acquire_operation(
            workspace.clone(),
            "python -m pytest",
            cargo_owner(ThreadId::new()),
            Some(ValidationAdmissionWait::new(None, cancellation.clone())),
        );
        tokio::pin!(pending);
        assert!(futures::poll!(&mut pending).is_pending());
        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(5), &mut pending)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        drop(external);
        drop(
            acquire_operation(
                workspace,
                "python -m pytest",
                cargo_owner(ThreadId::new()),
                Some(ValidationAdmissionWait::new(
                    Some(tokio::time::Instant::now() + Duration::from_secs(10)),
                    CancellationToken::new(),
                )),
            )
            .await
            .expect("cancelled admission must not retain the local gate"),
        );
    }

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
            .state
            .lock()
            .unwrap()
            .holder
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
        assert!(
            contender.is_err(),
            "competing validation must not be admitted"
        );
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
        let held =
            acquire_workspace_validation(temp.path(), "python -m pytest tests", first_python)
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
        assert!(
            foreign.contains("belongs to another session or agent"),
            "{foreign}"
        );
        let duplicate = acquire_workspace_validation(temp.path(), "python -m pytest", first_python)
            .await
            .unwrap_err();
        assert!(
            duplicate.contains("Resume the existing operation"),
            "{duplicate}"
        );
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
        assert!(
            foreign.contains("belongs to another session or agent"),
            "{foreign}"
        );
        assert!(
            !foreign.contains("Resume the existing operation"),
            "{foreign}"
        );
        drop(cargo);
        assert!(
            acquire_workspace_validation(temp.path(), "cargo check", cargo_owner(second))
                .await
                .is_ok()
        );
    }

    fn wait_budget(
        return_bound: Option<tokio::time::Instant>,
        cancellation: &CancellationToken,
    ) -> Option<ValidationAdmissionWait> {
        Some(ValidationAdmissionWait::new(
            return_bound,
            cancellation.clone(),
        ))
    }

    fn queued_contenders(workspace: &WorkspaceIdentity) -> usize {
        validation_gate(workspace.clone())
            .state
            .lock()
            .unwrap()
            .queue
            .len()
    }

    async fn wait_until_queued(workspace: &WorkspaceIdentity, expected: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while queued_contenders(workspace) != expected {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the launch must reach the validation queue");
    }

    // Saved sessions refused 34 `cargo check` launches whose foreign holder
    // released a median 53 s later; each refusal cost a model generation.
    #[tokio::test(start_paused = true)]
    async fn queued_validation_is_admitted_on_release_and_bounded_by_its_budget() {
        let workspace = WorkspaceIdentity::Local(PathBuf::from("queued-validation-budget"));
        let (holder, contender) = (cargo_owner(ThreadId::new()), cargo_owner(ThreadId::new()));
        let never = CancellationToken::new();
        let held = acquire_operation(workspace.clone(), "cargo check -p holder", holder, None)
            .await
            .unwrap();
        // Without a scoped budget, contention still returns at once.
        let immediate = acquire_operation(workspace.clone(), "cargo check", contender, None)
            .await
            .unwrap_err();
        assert!(!immediate.contains("queued this launch"), "{immediate}");
        assert!(immediate.contains("Do not poll"), "{immediate}");
        assert_eq!(queued_contenders(&workspace), 0);

        // A scoped launch queues instead of returning busy, then runs on release.
        let started = tokio::time::Instant::now();
        let pending = acquire_operation(
            workspace.clone(),
            "cargo check -p contender",
            contender,
            wait_budget(None, &never),
        );
        tokio::pin!(pending);
        assert!(futures::poll!(&mut pending).is_pending());
        assert_eq!(queued_contenders(&workspace), 1);
        tokio::time::advance(Duration::from_secs(45)).await;
        assert!(futures::poll!(&mut pending).is_pending());
        drop(held);
        let admitted = pending
            .await
            .expect("a released holder admits the queued launch");
        assert_eq!(started.elapsed(), Duration::from_secs(45));
        assert_eq!(queued_contenders(&workspace), 0);

        // A holder that never releases bounds the wait at the cap.
        let started = tokio::time::Instant::now();
        let busy = acquire_operation(
            workspace.clone(),
            "cargo check -p late",
            holder,
            wait_budget(None, &never),
        )
        .await
        .unwrap_err();
        assert_eq!(started.elapsed(), VALIDATION_ADMISSION_WAIT_CAP);
        assert!(busy.contains("cargo check -p contender"), "{busy}");
        assert!(
            busy.contains("queued this launch for 120.000 seconds"),
            "{busy}"
        );
        assert!(busy.contains("This validation did not run"), "{busy}");
        assert!(busy.contains("No process was started or queued"), "{busy}");
        assert!(
            busy.contains("belongs to another session or agent"),
            "{busy}"
        );
        assert_eq!(queued_contenders(&workspace), 0, "expiry leaves no claim");

        // The caller's return bound, less the launch reserve, ends the wait
        // sooner; a bound inside the reserve does not queue at all.
        let started = tokio::time::Instant::now();
        acquire_operation(
            workspace.clone(),
            "cargo check -p bounded",
            holder,
            wait_budget(Some(started + Duration::from_secs(20)), &never),
        )
        .await
        .unwrap_err();
        assert_eq!(started.elapsed(), Duration::from_secs(15));
        let started = tokio::time::Instant::now();
        let busy = acquire_operation(
            workspace.clone(),
            "cargo check -p reserved",
            holder,
            wait_budget(Some(started + Duration::from_secs(3)), &never),
        )
        .await
        .unwrap_err();
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert!(!busy.contains("queued this launch"), "{busy}");
        drop(admitted);
    }

    #[tokio::test(start_paused = true)]
    async fn queued_validation_is_fifo_cancellable_and_never_waits_on_its_own_thread() {
        let workspace = WorkspaceIdentity::Local(PathBuf::from("queued-validation-order"));
        let holder = cargo_owner(ThreadId::new());
        let (first, second, late) = (
            cargo_owner(ThreadId::new()),
            cargo_owner(ThreadId::new()),
            cargo_owner(ThreadId::new()),
        );
        let never = CancellationToken::new();
        let held = acquire_operation(workspace.clone(), "cargo test -p holder", holder, None)
            .await
            .unwrap();

        // The holder's own thread is told to resume it, never queued.
        let own = acquire_operation(
            workspace.clone(),
            "cargo check",
            holder,
            wait_budget(None, &never),
        )
        .await
        .unwrap_err();
        assert!(own.contains("Resume the existing operation"), "{own}");
        assert_eq!(queued_contenders(&workspace), 0);

        let cancel_first = CancellationToken::new();
        let first_wait = acquire_operation(
            workspace.clone(),
            "cargo check -p first",
            first,
            wait_budget(None, &cancel_first),
        );
        let second_wait = acquire_operation(
            workspace.clone(),
            "cargo check -p second",
            second,
            wait_budget(None, &never),
        );
        tokio::pin!(first_wait, second_wait);
        assert!(futures::poll!(&mut first_wait).is_pending());
        assert!(futures::poll!(&mut second_wait).is_pending());
        assert_eq!(queued_contenders(&workspace), 2);

        // Cancellation ends a wait at once and gives up its place in line.
        cancel_first.cancel();
        let cancelled = first_wait.await.unwrap_err();
        assert!(cancelled.contains("cancelled"), "{cancelled}");
        assert!(cancelled.contains("cargo test -p holder"), "{cancelled}");
        assert!(
            cancelled.contains("This validation did not run"),
            "{cancelled}"
        );
        assert_eq!(queued_contenders(&workspace), 1);
        assert!(
            futures::poll!(&mut second_wait).is_pending(),
            "the holder still owns the gate"
        );

        // A free gate goes to the queue front, not to a later arrival.
        drop(held);
        let barging = acquire_operation(workspace.clone(), "cargo check -p late", late, None)
            .await
            .unwrap_err();
        assert!(
            barging.contains("an earlier queued validation is admitted first"),
            "{barging}"
        );
        let second_permit = second_wait
            .await
            .expect("the queue front is admitted after release");
        let late_wait = acquire_operation(
            workspace.clone(),
            "cargo check -p late",
            late,
            wait_budget(None, &never),
        );
        tokio::pin!(late_wait);
        assert!(futures::poll!(&mut late_wait).is_pending());
        drop(second_permit);
        drop(
            late_wait
                .await
                .expect("the next launch is admitted in turn"),
        );
        assert_eq!(queued_contenders(&workspace), 0);
    }

    // A lost wakeup would strand a waiter until its 120 s cap. Releases race
    // both queue entry and a cancelled queue front handing the gate on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn release_racing_queue_entry_or_departure_never_strands_a_waiter() {
        let workspace = WorkspaceIdentity::Local(PathBuf::from("racing-validation"));
        let never = CancellationToken::new();
        let launch = |command: &'static str, cancellation: CancellationToken| {
            let workspace = workspace.clone();
            tokio::spawn(async move {
                acquire_operation(
                    workspace,
                    command,
                    cargo_owner(ThreadId::new()),
                    wait_budget(None, &cancellation),
                )
                .await
                .map(drop)
            })
        };
        for round in 0..200_u32 {
            let held = acquire_operation(
                workspace.clone(),
                "cargo check -p holder",
                cargo_owner(ThreadId::new()),
                None,
            )
            .await
            .unwrap();
            let waiter = launch("cargo check -p waiter", never.clone());
            let departing_cancel = CancellationToken::new();
            let departing = launch("cargo check -p departing", departing_cancel.clone());
            for _ in 0..round % 4 {
                tokio::task::yield_now().await;
            }
            let release = tokio::spawn(async move { drop(held) });
            departing_cancel.cancel();
            release.await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), waiter)
                .await
                .expect("a released gate must wake its queued waiter")
                .unwrap()
                .expect("the waiter is admitted, not refused");
            let _ = tokio::time::timeout(Duration::from_secs(10), departing)
                .await
                .expect("a cancelled contender must settle");
            assert_eq!(queued_contenders(&workspace), 0, "round {round}");
        }
    }

    // A queued launch keeps its turn's workspace lease, so another call of
    // that turn would wait behind it (a direct call without any bound).
    #[tokio::test(start_paused = true)]
    async fn queued_validation_yields_to_a_call_waiting_on_its_turn() {
        let workspace = WorkspaceIdentity::Local(PathBuf::from("yielding-validation"));
        let (holder, contender) = (cargo_owner(ThreadId::new()), cargo_owner(ThreadId::new()));
        let never = CancellationToken::new();
        let turn = Arc::new(TurnAdmissionWaiters::default());
        let yielding = || wait_budget(None, &never).map(|wait| wait.yielding_to(Arc::clone(&turn)));

        // Admission counts as waiting only while it is blocked.
        assert_eq!(turn.track(async { 7 }).await, 7);
        turn.track(tokio::task::yield_now()).await;
        assert_eq!(turn.waiting.load(Ordering::SeqCst), 0);

        let held = acquire_operation(workspace.clone(), "cargo check -p holder", holder, None)
            .await
            .unwrap();
        let queued = acquire_operation(
            workspace.clone(),
            "cargo check -p queued",
            contender,
            yielding(),
        );
        tokio::pin!(queued);
        assert!(futures::poll!(&mut queued).is_pending());
        assert_eq!(queued_contenders(&workspace), 1);

        // A call of the same turn blocks on admission: the queued launch stops
        // queueing at once and gives up its place in line.
        let started = tokio::time::Instant::now();
        let mut sibling = Box::pin(turn.track(std::future::pending::<()>()));
        assert!(futures::poll!(sibling.as_mut()).is_pending());
        let busy = queued.await.unwrap_err();
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert!(
            busy.contains("then stopped because another call in this turn is waiting"),
            "{busy}"
        );
        assert!(busy.contains("This validation did not run"), "{busy}");
        assert!(
            busy.contains("without concurrent calls in its cell"),
            "{busy}"
        );
        assert_eq!(queued_contenders(&workspace), 0);

        // While that call waits, a launch does not queue, but a free gate
        // still admits it: yielding only ends queueing.
        let busy = acquire_operation(
            workspace.clone(),
            "cargo check -p again",
            contender,
            yielding(),
        )
        .await
        .unwrap_err();
        assert!(
            busy.contains("did not queue this launch because another call in this turn"),
            "{busy}"
        );
        assert_eq!(queued_contenders(&workspace), 0);
        drop(held);
        let admitted = acquire_operation(
            workspace.clone(),
            "cargo check -p free",
            contender,
            yielding(),
        )
        .await
        .expect("a free gate admits regardless of waiting calls");

        // Once the waiting call leaves, a lone launch queues again.
        drop(sibling);
        let queued = acquire_operation(
            workspace.clone(),
            "cargo check -p after",
            holder,
            yielding(),
        );
        tokio::pin!(queued);
        assert!(futures::poll!(&mut queued).is_pending());
        assert_eq!(queued_contenders(&workspace), 1);
        drop(admitted);
        drop(
            queued
                .await
                .expect("the queued launch is admitted on release"),
        );
        assert_eq!(queued_contenders(&workspace), 0);
    }

    // A lost arrival wakeup would block the waiting call until the queued
    // launch's 120 s cap, although nothing ever releases the holder here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn turn_waiter_racing_queue_entry_always_ends_the_wait() {
        let workspace = WorkspaceIdentity::Local(PathBuf::from("racing-turn-waiter"));
        let never = CancellationToken::new();
        let held = acquire_operation(
            workspace.clone(),
            "cargo check -p holder",
            cargo_owner(ThreadId::new()),
            None,
        )
        .await
        .unwrap();
        for round in 0..200_u32 {
            let turn = Arc::new(TurnAdmissionWaiters::default());
            let launch = tokio::spawn({
                let (workspace, wait) = (
                    workspace.clone(),
                    wait_budget(None, &never).map(|wait| wait.yielding_to(Arc::clone(&turn))),
                );
                async move {
                    acquire_operation(
                        workspace,
                        "cargo check -p queued",
                        cargo_owner(ThreadId::new()),
                        wait,
                    )
                    .await
                    .map(drop)
                }
            });
            for _ in 0..round % 4 {
                tokio::task::yield_now().await;
            }
            let sibling = tokio::spawn(async move {
                turn.track(std::future::pending::<()>()).await;
            });
            let busy = tokio::time::timeout(Duration::from_secs(10), launch)
                .await
                .expect("a waiting call of the turn must end the queued wait")
                .unwrap()
                .unwrap_err();
            assert!(busy.contains("another call in this turn"), "{busy}");
            sibling.abort();
            assert_eq!(queued_contenders(&workspace), 0, "round {round}");
        }
        drop(held);
    }

    /// Mirrors `exec_command`'s cancellation contract: dispatch cancels the
    /// runtime token, and the aborted response waits for handler cleanup.
    struct ValidationProbe(PathBuf);

    impl codex_tools::ToolExecutor<crate::tools::context::ToolInvocation> for ValidationProbe {
        fn tool_name(&self) -> codex_tools::ToolName {
            codex_tools::ToolName::plain("validation_probe")
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: "validation_probe".to_string(),
                description: "Validation admission probe.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })
        }

        fn supports_parallel_tool_calls(&self) -> bool {
            true
        }

        fn handle(
            &self,
            invocation: crate::tools::context::ToolInvocation,
        ) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(async move {
                let environment = codex_exec_server::Environment::default_for_tests();
                let cwd = codex_utils_path_uri::PathUri::from_host_native_path(&self.0).unwrap();
                let permit = acquire_validation_operation(
                    &environment,
                    &cwd,
                    "cargo check -p probe",
                    cargo_owner(invocation.session.thread_id()),
                )
                .await
                .map_err(crate::FunctionCallError::RespondToModel)?;
                drop(permit);
                Ok(
                    Box::new(crate::tools::context::FunctionToolOutput::from_text(
                        "admitted".to_string(),
                        Some(true),
                    )) as Box<dyn crate::tools::context::ToolOutput>,
                )
            })
        }
    }

    impl crate::tools::registry::CoreToolRuntime for ValidationProbe {
        fn waits_for_runtime_cancellation(&self) -> bool {
            true
        }

        fn owns_unified_exec_processes(&self) -> bool {
            true
        }
    }

    /// Without parallel support, a tool needs its turn's exclusive lease.
    struct ExclusiveSibling;

    impl codex_tools::ToolExecutor<crate::tools::context::ToolInvocation> for ExclusiveSibling {
        fn tool_name(&self) -> codex_tools::ToolName {
            codex_tools::ToolName::plain("exclusive_sibling")
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: "exclusive_sibling".to_string(),
                description: "Exclusive workspace sibling.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })
        }

        fn handle(
            &self,
            _invocation: crate::tools::context::ToolInvocation,
        ) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(async {
                Ok(
                    Box::new(crate::tools::context::FunctionToolOutput::from_text(
                        "sibling ran".to_string(),
                        Some(true),
                    )) as Box<dyn crate::tools::context::ToolOutput>,
                )
            })
        }
    }

    impl crate::tools::registry::CoreToolRuntime for ExclusiveSibling {}

    async fn probe_runtime(workspace: &Path) -> crate::tools::parallel::ToolCallRuntime {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let router = Arc::new(crate::tools::router::ToolRouter::from_parts(
            crate::tools::registry::ToolRegistry::from_tools([
                Arc::new(ValidationProbe(workspace.to_path_buf()))
                    as Arc<dyn crate::tools::registry::CoreToolRuntime>,
                Arc::new(ExclusiveSibling) as Arc<dyn crate::tools::registry::CoreToolRuntime>,
            ]),
            Vec::new(),
        ));
        crate::tools::parallel::ToolCallRuntime::new(
            Arc::new(session),
            crate::session::step_context::StepContext::for_test(Arc::new(turn))
                .with_tool_router_for_test(router),
            Arc::new(tokio::sync::Mutex::new(
                crate::turn_diff_tracker::TurnDiffTracker::new(),
            )),
        )
    }

    fn dispatch_tool(
        runtime: &crate::tools::parallel::ToolCallRuntime,
        tool: &str,
        id: &str,
        source: crate::tools::context::ToolCallSource,
        cancellation: CancellationToken,
    ) -> tokio::task::JoinHandle<
        Result<crate::tools::registry::AnyToolResult, crate::FunctionCallError>,
    > {
        tokio::spawn(runtime.clone().handle_tool_call_with_source(
            crate::tools::router::ToolCall {
                tool_name: codex_tools::ToolName::plain(tool),
                call_id: id.to_string(),
                payload: crate::tools::context::ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            },
            source,
            cancellation,
        ))
    }

    fn nested_source(id: &str) -> crate::tools::context::ToolCallSource {
        crate::tools::context::ToolCallSource::CodeMode {
            cell_id: "validation-cell".to_string(),
            parent_call_id: None,
            runtime_tool_call_id: id.to_string(),
            cancellation_cause: None,
            nested_deadline: Some(Instant::now() + Duration::from_secs(60)),
        }
    }

    fn tool_outcome(
        result: Result<crate::tools::registry::AnyToolResult, crate::FunctionCallError>,
    ) -> String {
        match result {
            Ok(result) => result.result.code_mode_result(&result.payload).to_string(),
            Err(error) => error.to_string(),
        }
    }

    #[tokio::test]
    async fn dispatch_queues_validation_behind_a_foreign_holder() {
        use crate::tools::context::ToolCallSource;

        let workspace = tempfile::tempdir().unwrap();
        let identity = local_identity(workspace.path()).await;
        let runtime = probe_runtime(workspace.path()).await;
        let dispatch = |id: &str, source: ToolCallSource, cancellation: CancellationToken| {
            dispatch_tool(&runtime, "validation_probe", id, source, cancellation)
        };
        let (nested, outcome) = (nested_source, tool_outcome);
        let held = acquire_workspace_validation(
            workspace.path(),
            "cargo test -p holder",
            cargo_owner(ThreadId::new()),
        )
        .await
        .unwrap();

        // A direct launch queues behind the foreign holder instead of returning
        // a busy result the model would poll, and runs on release.
        let direct = dispatch("direct", ToolCallSource::Direct, CancellationToken::new());
        wait_until_queued(&identity, 1).await;
        assert!(
            !direct.is_finished(),
            "a queued direct launch must not return busy"
        );
        drop(held);
        let direct = tokio::time::timeout(Duration::from_secs(10), direct)
            .await
            .expect("release must admit the queued direct launch")
            .unwrap();
        let direct = outcome(direct);
        assert!(direct.contains("admitted"), "{direct}");
        let held = acquire_workspace_validation(
            workspace.path(),
            "cargo test -p holder",
            cargo_owner(ThreadId::new()),
        )
        .await
        .unwrap();

        // A code-mode launch queues behind the foreign holder and runs on release.
        let queued = dispatch("queued", nested("queued"), CancellationToken::new());
        wait_until_queued(&identity, 1).await;
        assert!(
            !queued.is_finished(),
            "a queued launch must not return busy"
        );
        drop(held);
        let admitted = tokio::time::timeout(Duration::from_secs(10), queued)
            .await
            .expect("release must admit the queued launch")
            .unwrap();
        let admitted = outcome(admitted);
        assert!(admitted.contains("admitted"), "{admitted}");

        // Interrupting a queued launch ends its wait promptly, without waiting
        // for the cleanup deadline, and leaves the holder's lease intact.
        let held = acquire_workspace_validation(
            workspace.path(),
            "cargo test -p holder",
            cargo_owner(ThreadId::new()),
        )
        .await
        .unwrap();
        let cancellation = CancellationToken::new();
        let interrupted = dispatch("interrupted", nested("interrupted"), cancellation.clone());
        wait_until_queued(&identity, 1).await;
        cancellation.cancel();
        let interrupted = tokio::time::timeout(Duration::from_secs(10), interrupted)
            .await
            .expect("an interrupted launch must not wait out its admission budget")
            .unwrap();
        let interrupted = outcome(interrupted);
        assert!(!interrupted.contains("admitted"), "{interrupted}");
        assert_eq!(queued_contenders(&identity), 0);
        assert!(
            acquire_workspace_validation(
                workspace.path(),
                "cargo check",
                cargo_owner(ThreadId::new())
            )
            .await
            .is_err(),
            "the holder keeps its lease"
        );
        drop(held);
    }

    // Saved concurrent cells paired a refused `cargo check` with a read or a
    // syntax check that ran at once. A queued launch must not hold that call
    // behind its lease: a direct sibling would wait for the whole budget.
    #[tokio::test]
    async fn queued_code_mode_validation_yields_to_a_sibling_waiting_on_its_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let identity = local_identity(workspace.path()).await;
        let runtime = probe_runtime(workspace.path()).await;
        let held = acquire_workspace_validation(
            workspace.path(),
            "cargo test -p holder",
            cargo_owner(ThreadId::new()),
        )
        .await
        .unwrap();
        let queued = dispatch_tool(
            &runtime,
            "validation_probe",
            "queued",
            nested_source("queued"),
            CancellationToken::new(),
        );
        wait_until_queued(&identity, 1).await;
        let sibling = dispatch_tool(
            &runtime,
            "exclusive_sibling",
            "sibling",
            crate::tools::context::ToolCallSource::Direct,
            CancellationToken::new(),
        );
        let (queued, sibling) = tokio::time::timeout(Duration::from_secs(10), async {
            (queued.await, sibling.await)
        })
        .await
        .expect("the queued launch must stop blocking its waiting sibling");
        let queued = tool_outcome(queued.unwrap());
        assert!(
            queued.contains("another call in this turn is waiting"),
            "{queued}"
        );
        assert!(queued.contains("This validation did not run"), "{queued}");
        let sibling = tool_outcome(sibling.unwrap());
        assert!(sibling.contains("sibling ran"), "{sibling}");
        assert_eq!(queued_contenders(&identity), 0);
        assert!(
            acquire_workspace_validation(
                workspace.path(),
                "cargo check",
                cargo_owner(ThreadId::new())
            )
            .await
            .is_err(),
            "the foreign holder keeps its lease"
        );
        drop(held);
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

    /// Audit 151: queued validation admission is persisted with the waiting
    /// dispatch; immediate admission and an unqueued refusal are not waits.
    #[tokio::test(start_paused = true)]
    async fn audit151_queued_validation_admission_joins_the_dispatch_profile() {
        use crate::tools::tool_dispatch_trace::ToolDispatchTiming;
        use crate::tools::tool_dispatch_trace::scope_tool_dispatch_timing;

        let workspace = WorkspaceIdentity::Local(PathBuf::from("audit151-recorded-validation-wait"));
        let never = CancellationToken::new();
        let timing = Arc::new(ToolDispatchTiming::new(tokio::time::Instant::now(), false));
        let held = scope_tool_dispatch_timing(
            Arc::clone(&timing),
            acquire_operation(
                workspace.clone(),
                "cargo check -p holder",
                cargo_owner(ThreadId::new()),
                None,
            ),
        )
        .await
        .unwrap();
        scope_tool_dispatch_timing(
            Arc::clone(&timing),
            acquire_operation(
                workspace.clone(),
                "cargo check",
                cargo_owner(ThreadId::new()),
                None,
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(recorded_waits(&timing), Vec::new());

        let started = tokio::time::Instant::now();
        scope_tool_dispatch_timing(
            Arc::clone(&timing),
            acquire_operation(
                workspace.clone(),
                "cargo check -p bounded",
                cargo_owner(ThreadId::new()),
                wait_budget(Some(started + Duration::from_secs(20)), &never),
            ),
        )
        .await
        .unwrap_err();
        let cancellation = CancellationToken::new();
        let cancelled = scope_tool_dispatch_timing(
            Arc::clone(&timing),
            acquire_operation(
                workspace.clone(),
                "cargo check -p cancelled",
                cargo_owner(ThreadId::new()),
                wait_budget(None, &cancellation),
            ),
        );
        tokio::pin!(cancelled);
        assert!(futures::poll!(&mut cancelled).is_pending());
        tokio::time::advance(Duration::from_secs(3)).await;
        cancellation.cancel();
        let error = cancelled.await.unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        assert_eq!(
            recorded_waits(&timing),
            vec![
                (
                    "validation_gate".to_string(),
                    ToolLifecycleWakeReason::Timeout,
                    Some(15_000),
                    Some(15_000),
                ),
                (
                    "validation_gate".to_string(),
                    ToolLifecycleWakeReason::Cancelled,
                    Some(120_000),
                    Some(3_000),
                ),
            ]
        );
        drop(held);
    }
}
