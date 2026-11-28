use std::collections::HashMap;
use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::host::EncodedFrame;
use codex_code_mode_protocol::host::RequestId;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(in crate::remote_session) use self::cleanup::SessionCleanup;
pub(super) use self::cell_ids::remote_cell_id;
use self::delegate_runtime::DelegateRuntime;
use self::request_tracker::RequestTracker;
use self::session_registry::SessionRegistry;
pub(super) use self::types::DriverCommand;
pub(super) use self::types::DriverEvent;
pub(in crate::remote_session) use self::types::RemoteSession;

mod cell_ids;
mod cleanup;
mod commands;
mod delegate_runtime;
mod request_tracker;
mod responses;
mod session_registry;
mod types;

pub(super) struct DriverLifecycle {
    pub(super) alive: Arc<AtomicBool>,
    pub(super) failure: Arc<std::sync::Mutex<Option<String>>>,
    pub(super) cancellation: CancellationToken,
}

/// Execute-result claims in caller submission order. Only IDs backed by a
/// tracker obligation can enter the queue; tracker retirement revokes even a
/// claim that is already queued. Each eligible ID is charged to a tracker
/// operation, and stale submissions never consume capacity.
#[derive(Default)]
pub(super) struct ExecuteClaims {
    state: Mutex<ExecuteClaimState>,
    ready: Notify,
}

#[derive(Default)]
struct ExecuteClaimState {
    closed: bool,
    // Whether each eligible request ID has already been queued.
    live: HashMap<RequestId, bool>,
    queued: VecDeque<RequestId>,
}

impl ExecuteClaims {
    fn state(&self) -> MutexGuard<'_, ExecuteClaimState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(&self, id: RequestId) {
        let replaced = self.state().live.insert(id, false);
        debug_assert!(replaced.is_none());
    }

    pub(super) fn submit(&self, id: RequestId) -> bool {
        let mut state = self.state();
        if state.closed {
            return false;
        }
        let Some(queued) = state.live.get_mut(&id) else {
            // Shutdown can retire the row before the caller sees the result.
            // Such a claim was already a no-op in the driver.
            return true;
        };
        if !*queued {
            *queued = true;
            state.queued.push_back(id);
            drop(state);
            self.ready.notify_one();
        }
        true
    }

    fn retire(&self, id: RequestId) {
        let mut state = self.state();
        if state.live.remove(&id) == Some(true) {
            state.queued.retain(|queued| *queued != id);
        }
    }

    async fn recv(&self) -> RequestId {
        loop {
            {
                let mut state = self.state();
                if let Some(id) = state.queued.pop_front() {
                    state.live.remove(&id);
                    return id;
                }
            }
            // The single receiver rechecks the queue after a wake; notify_one
            // also keeps a permit for a submission before it starts waiting.
            self.ready.notified().await;
        }
    }

    fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        state.live.clear();
        state.queued.clear();
    }
}

pub(super) struct ConnectionDriver {
    pub(super) tool_catalog_references: bool,
    pub(super) named_state_snapshots: bool,
    pub(super) receipt_recovery: bool,
    pub(super) timeout_pause: bool,
    catalogs: std::collections::HashMap<
        codex_code_mode_protocol::host::SessionId,
        (u64, Arc<[codex_code_mode_protocol::ToolDefinition]>),
    >,
    command_rx: mpsc::Receiver<DriverCommand>,
    event_rx: mpsc::Receiver<DriverEvent>,
    event_tx: mpsc::Sender<DriverEvent>,
    execute_claims: Arc<ExecuteClaims>,
    outgoing_tx: mpsc::Sender<EncodedFrame>,
    requests: RequestTracker,
    sessions: SessionRegistry,
    delegates: DelegateRuntime,
    alive: Arc<AtomicBool>,
    failure: Arc<std::sync::Mutex<Option<String>>>,
    cancellation: CancellationToken,
    failed: bool,
    // Last scheduler observation for backend-stall accounting while a control
    // deadline is armed (charged per deadline in `RequestTracker`); see
    // `within_host_budget` in the parent module for the same accounting.
    stall_observed: tokio::time::Instant,
}

type BackendStallProbe =
    std::pin::Pin<Box<dyn std::future::Future<Output = std::time::Duration> + Send>>;

/// Waits at most `BACKEND_STALL_EVIDENCE` for a no-op on the blocking pool,
/// which Windows child pipes share, and returns how long that took.
fn backend_stall_probe() -> BackendStallProbe {
    Box::pin(async {
        let started = tokio::time::Instant::now();
        let _ = tokio::time::timeout(
            super::BACKEND_STALL_EVIDENCE,
            tokio::task::spawn_blocking(|| ()),
        )
        .await;
        started.elapsed()
    })
}

impl ConnectionDriver {
    pub(super) fn new(
        command_rx: mpsc::Receiver<DriverCommand>,
        event_rx: mpsc::Receiver<DriverEvent>,
        event_tx: mpsc::Sender<DriverEvent>,
        outgoing_tx: mpsc::Sender<EncodedFrame>,
        lifecycle: DriverLifecycle,
    ) -> (Self, Arc<ExecuteClaims>) {
        let execute_claims = Arc::new(ExecuteClaims::default());
        (
            Self {
                tool_catalog_references: false,
                named_state_snapshots: false,
                receipt_recovery: false,
                timeout_pause: false,
                catalogs: std::collections::HashMap::new(),
                command_rx,
                event_rx,
                event_tx: event_tx.clone(),
                execute_claims: Arc::clone(&execute_claims),
                outgoing_tx,
                requests: RequestTracker::new(Arc::clone(&execute_claims)),
                sessions: SessionRegistry::new(),
                delegates: DelegateRuntime::new(event_tx),
                alive: lifecycle.alive,
                failure: lifecycle.failure,
                cancellation: lifecycle.cancellation,
                failed: false,
                stall_observed: tokio::time::Instant::now(),
            },
            execute_claims,
        )
    }

    pub(super) async fn run(mut self) {
        enum Ready {
            ControlTimeout,
            StallProbe(std::time::Duration),
            StallTick,
            Event(Option<DriverEvent>),
            Claim(RequestId),
            AbandonedShutdownRetry(mpsc::OwnedPermit<EncodedFrame>),
            Command(Option<DriverCommand>),
        }
        let mut ticks = tokio::time::interval(super::BACKEND_STALL_TICK);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Runs beside traffic rather than inline, so an acknowledgement that
        // arrives while it waits still settles its request.
        let mut stall_probe: Option<BackendStallProbe> = None;
        loop {
            let deadline = self.requests.next_control_deadline();
            let probing = stall_probe.is_some();
            let close_retry_at = if self.alive.load(Ordering::Acquire) {
                self.requests.next_abandoned_shutdown_retry()
            } else {
                None
            };
            let ready = tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => {
                    self.fail("code-mode host connection closed".to_string());
                    return;
                }
                waited = async {
                    match stall_probe.as_mut() {
                        Some(probe) => probe.await,
                        None => std::future::pending().await,
                    }
                } => Ready::StallProbe(waited),
                _ = async {
                    match deadline {
                        Some(deadline) if !probing => tokio::time::sleep_until(deadline).await,
                        _ => std::future::pending::<()>().await,
                    }
                } => Ready::ControlTimeout,
                _ = ticks.tick(), if deadline.is_some() && !probing => Ready::StallTick,
                ready = async {
                    // Cancellation stays first; traffic channels share the
                    // default fair selector rather than event-first priority.
                    tokio::select! {
                        event = self.event_rx.recv() => Ready::Event(event),
                        claim = tokio::task::coop::cooperative(self.execute_claims.recv()) => Ready::Claim(claim),
                        command = self.command_rx.recv() => Ready::Command(command),
                        permit = async {
                            if let Some(at) = close_retry_at {
                                tokio::time::sleep_until(at).await;
                                if let Ok(permit) = self.outgoing_tx.clone().reserve_owned().await {
                                    return permit;
                                }
                            }
                            // Writer loss is retired by the supervisor after
                            // the reader drains, not by a newly due retry.
                            std::future::pending().await
                        } => Ready::AbandonedShutdownRetry(permit),
                    }
                } => ready,
            };
            // Only a gap spent with a deadline armed, and not inside a probe
            // (which measures its own interval), is backend stall; time the
            // driver sat idle before a deadline existed is not.
            if deadline.is_some() && !probing {
                let mut stalled = std::time::Duration::ZERO;
                super::note_backend_gap(&mut self.stall_observed, &mut stalled);
                self.requests.note_control_stall(stalled, self.stall_observed);
            } else {
                self.stall_observed = tokio::time::Instant::now();
            }
            match ready {
                Ready::StallTick => {}
                Ready::ControlTimeout => stall_probe = Some(backend_stall_probe()),
                Ready::StallProbe(waited) => {
                    stall_probe = None;
                    // Host messages already received win over the expired
                    // window, as the request does in `within_host_budget`.
                    for _ in 0..self.event_rx.len() {
                        let Ok(event) = self.event_rx.try_recv() else {
                            break;
                        };
                        if !self.cancel_dropped_callers() || !self.handle_event(event) {
                            return;
                        }
                    }
                    if !self.backend_stalled_through_deadline(waited) {
                        self.fail("code-mode destructive cleanup acknowledgement timed out; external effects are unknown; do not replay work".to_string());
                        return;
                    }
                }
                Ready::Event(event) => {
                    let Some(event) = event else {
                        self.fail("code-mode host event stream closed".to_string());
                        return;
                    };
                    if !self.cancel_dropped_callers() || !self.handle_event(event) {
                        return;
                    }
                }
                Ready::Claim(request_id) => self.requests.claim_execute(request_id),
                Ready::AbandonedShutdownRetry(permit) => {
                    if self.alive.load(Ordering::Acquire) && !self.retry_abandoned_shutdown(permit) {
                        return;
                    }
                }
                Ready::Command(command) => {
                    let Some(command) = command else {
                        self.fail("code-mode host command stream closed".to_string());
                        return;
                    };
                    if !self.cancel_dropped_callers() || !self.handle_command(command) {
                        return;
                    }
                }
            }
        }
    }

    /// After a control deadline's blocking-pool probe, re-arms the expired
    /// windows (bounded per request) in which this backend measurably stalled:
    /// late scheduler ticks, or a probe (Windows child pipes share that pool)
    /// that could not start. A host is not charged with time its client could
    /// not observe. False when an expired window has no such evidence.
    fn backend_stalled_through_deadline(&mut self, probe_wait: std::time::Duration) -> bool {
        let now = tokio::time::Instant::now();
        if probe_wait > super::BACKEND_STALL_TICK {
            self.requests.note_control_stall(probe_wait, now);
        }
        self.stall_observed = now;
        let Some(stalled) = self.requests.rearm_expired_control_deadlines(now) else {
            return false;
        };
        if !stalled.is_zero() {
            tracing::warn!(
                stalled = ?stalled,
                "code-mode backend stalled during a cleanup acknowledgement window; re-arming it"
            );
        }
        true
    }

    fn handle_event(&mut self, event: DriverEvent) -> bool {
        let keep_running = match event {
            DriverEvent::HostMessage(message) => self.handle_host_message(message),
            DriverEvent::DelegateCompleted { id, result } => self.complete_delegate(id, result),
            DriverEvent::DelegateTimeoutPause { id, paused } => self.pause_delegate_timeout(id, paused),
            DriverEvent::RequestCancelled(id) => self.cancel_request(id),
            DriverEvent::Failed(reason) => {
                self.fail(reason);
                false
            }
        };
        if keep_running {
            self.flush_deferred_waits()
        } else {
            false
        }
    }

    fn admission_error(&self) -> Option<String> {
        if !self.alive.load(Ordering::Acquire) {
            return Some("code-mode host connection is retiring".into());
        }
        let reserved = codex_code_mode_protocol::host::MAX_IN_FLIGHT_REQUESTS
            + codex_code_mode_protocol::host::MAX_PENDING_DELEGATE_REQUESTS;
        (self.requests.operation_count() >= codex_code_mode_protocol::host::MAX_IN_FLIGHT_REQUESTS
            || self.outgoing_tx.capacity() <= reserved)
            .then(|| "code-mode host request capacity reached; retry after existing work progresses; no request was sent".into())
    }

    fn queue_frame(&mut self, frame: EncodedFrame) -> bool {
        match self.outgoing_tx.try_send(frame) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.fail("code-mode host outgoing queue is full".to_string());
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.fail("code-mode host writer closed".to_string());
                false
            }
        }
    }

    fn fail(&mut self, reason: String) {
        if self.failed {
            return;
        }
        self.failed = true;
        self.alive.store(false, Ordering::Release);
        let reason = {
            let mut failure = self
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            failure.get_or_insert(reason).clone()
        };
        self.requests.fail_all(&reason);
        let failed_sessions = self.sessions.drain();
        self.delegates.fail_all(failed_sessions);
        self.cancellation.cancel();
    }
}

impl Drop for ConnectionDriver {
    fn drop(&mut self) {
        self.fail("code-mode connection driver stopped unexpectedly".to_string());
    }
}

fn notify_cell_closed(delegate: &Arc<dyn CodeModeSessionDelegate>, cell_id: &CellId) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| delegate.cell_closed(cell_id)));
}
