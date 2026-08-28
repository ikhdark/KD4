//! Adapter between core tool dispatch objects and rollout-trace events.
//!
//! `codex-rollout-trace` owns the event schema and writer behavior. This module
//! keeps the core-specific mapping from registry invocations/results out of the
//! registry control flow.

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;

use crate::FunctionCallError;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::turn_timing::TurnTimingState;
use codex_protocol::protocol::SamplingGenerationId;
use codex_protocol::protocol::ToolExecutionId;
use codex_protocol::protocol::ToolLifecycleBoundary;
use codex_protocol::protocol::ToolLifecycleTimerWait;
use codex_protocol::protocol::TurnTimingToolLifecycleEvent;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputSkipDisposition;

static NEXT_TOOL_EXECUTION_ID: AtomicU64 = AtomicU64::new(1);

tokio::task_local! {
    static ACTIVE_TOOL_DISPATCH_TIMING: Arc<ToolDispatchTiming>;
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ToolDispatchTimingSnapshot {
    pub execution_id: ToolExecutionId,
    pub sampling_generation_id: SamplingGenerationId,
    pub lifecycle_events: Vec<TurnTimingToolLifecycleEvent>,
    pub timer_waits: Vec<ToolLifecycleTimerWait>,
    pub retry_count: u32,
    pub reentry_count: u32,
    pub outcome: Option<&'static str>,
    pub first_poll_at_ms: Option<u64>,
    pub item_to_first_poll_ms: Option<u64>,
    pub parallel_gate_wait_ms: Option<u64>,
    pub authorization_state_coordination_ms: Option<u64>,
    pub first_poll_to_handler_entry_ms: Option<u64>,
    pub handler_duration_ms: Option<u64>,
    pub workspace_evidence_before_ms: Option<u64>,
    pub workspace_evidence_before_cache_hit: Option<bool>,
    pub workspace_evidence_before_timed_out_git_dependencies: Vec<String>,
    pub workspace_evidence_after_ms: Option<u64>,
    pub pre_tool_hook_ms: Option<u64>,
    pub post_tool_hook_ms: Option<u64>,
    pub output_projection_ms: Option<u64>,
    pub history_persistence_ms: Option<u64>,
    pub first_poll_to_output_collected_ms: Option<u64>,
    pub exec_request_to_spawn_ms: Option<u64>,
    pub exec_spawn_to_exit_ms: Option<u64>,
    pub exec_exit_to_delivery_ms: Option<u64>,
    pub exec_spawn_to_delivery_ms: Option<u64>,
    pub exec_process_alive_at_delivery: bool,
    pub exec_cleanup_state_observed: bool,
    pub exec_background_process_expected: bool,
    pub exec_running_process_after_cleanup: bool,
    pub post_handler_ms: Option<u64>,
    pub total_duration_ms: Option<u64>,
    pub parallel_gate_admitted: bool,
    pub eager: bool,
}

#[derive(Debug)]
pub(crate) struct ToolDispatchTiming {
    turn_timing: Option<Arc<TurnTimingState>>,
    execution_id: ToolExecutionId,
    sampling_generation_id: SamplingGenerationId,
    lifecycle_events: StdMutex<Vec<TurnTimingToolLifecycleEvent>>,
    timer_waits: StdMutex<Vec<ToolLifecycleTimerWait>>,
    retry_count: AtomicU32,
    reentry_count: AtomicU32,
    item_accepted_at: Instant,
    first_poll_at: OnceLock<Instant>,
    first_poll_at_ms: OnceLock<u64>,
    parallel_gate_admitted_at: OnceLock<Instant>,
    authorization_state_coordination: OnceLock<Duration>,
    handler_entry_at: OnceLock<Instant>,
    handler_exit_at: OnceLock<Instant>,
    workspace_evidence_before: OnceLock<Duration>,
    workspace_evidence_before_cache_hit: OnceLock<bool>,
    workspace_evidence_before_timed_out_git_dependencies: OnceLock<Vec<String>>,
    workspace_evidence_after: OnceLock<Duration>,
    pre_tool_hook: OnceLock<Duration>,
    post_tool_hook: OnceLock<Duration>,
    output_projection: OnceLock<Duration>,
    history_persistence: OnceLock<Duration>,
    output_collected_at: OnceLock<Instant>,
    exec_process_spawned_at: OnceLock<Instant>,
    exec_process_exited_at: OnceLock<Instant>,
    outcome: OnceLock<&'static str>,
    exec_cleanup_state_recorded: AtomicBool,
    exec_background_process_expected: AtomicBool,
    exec_running_process_after_cleanup: AtomicBool,
    eager: bool,
}

impl ToolDispatchTiming {
    #[cfg(test)]
    pub(crate) fn new(item_accepted_at: Instant, eager: bool) -> Self {
        Self::new_inner(None, item_accepted_at, eager)
    }

    pub(crate) fn new_with_turn_clock(
        turn_timing: Arc<TurnTimingState>,
        item_accepted_at: Instant,
        eager: bool,
    ) -> Self {
        Self::new_inner(Some(turn_timing), item_accepted_at, eager)
    }

    fn new_inner(
        turn_timing: Option<Arc<TurnTimingState>>,
        item_accepted_at: Instant,
        eager: bool,
    ) -> Self {
        let execution_id = ToolExecutionId(format!(
            "tool-execution-{}",
            NEXT_TOOL_EXECUTION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let sampling_generation_id = SamplingGenerationId(
            turn_timing
                .as_ref()
                .map(|timing| timing.sampling_generation_id())
                .unwrap_or_else(|| "generation-pending".to_string()),
        );
        let timing = Self {
            turn_timing,
            execution_id,
            sampling_generation_id,
            lifecycle_events: StdMutex::new(Vec::new()),
            timer_waits: StdMutex::new(Vec::new()),
            retry_count: AtomicU32::new(0),
            reentry_count: AtomicU32::new(0),
            item_accepted_at,
            first_poll_at: OnceLock::new(),
            first_poll_at_ms: OnceLock::new(),
            parallel_gate_admitted_at: OnceLock::new(),
            authorization_state_coordination: OnceLock::new(),
            handler_entry_at: OnceLock::new(),
            handler_exit_at: OnceLock::new(),
            workspace_evidence_before: OnceLock::new(),
            workspace_evidence_before_cache_hit: OnceLock::new(),
            workspace_evidence_before_timed_out_git_dependencies: OnceLock::new(),
            workspace_evidence_after: OnceLock::new(),
            pre_tool_hook: OnceLock::new(),
            post_tool_hook: OnceLock::new(),
            output_projection: OnceLock::new(),
            history_persistence: OnceLock::new(),
            output_collected_at: OnceLock::new(),
            exec_process_spawned_at: OnceLock::new(),
            exec_process_exited_at: OnceLock::new(),
            outcome: OnceLock::new(),
            exec_cleanup_state_recorded: AtomicBool::new(false),
            exec_background_process_expected: AtomicBool::new(false),
            exec_running_process_after_cleanup: AtomicBool::new(false),
            eager,
        };
        timing.record_boundary(ToolLifecycleBoundary::RequestCreated);
        timing
    }

    pub(crate) fn execution_id(&self) -> &ToolExecutionId {
        &self.execution_id
    }

    pub(crate) fn turn_timing_state(&self) -> Option<Arc<TurnTimingState>> {
        self.turn_timing.as_ref().map(Arc::clone)
    }

    pub(crate) fn record_boundary(&self, boundary: ToolLifecycleBoundary) -> bool {
        let Some(turn_timing) = self.turn_timing.as_ref() else {
            return false;
        };
        let mut events = self
            .lifecycle_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if events.iter().any(|event| event.boundary == boundary) {
            return false;
        }
        events.push(TurnTimingToolLifecycleEvent {
            boundary,
            at_ms: turn_timing.monotonic_offset_ms(),
            context: turn_timing.lifecycle_context(),
            retry_count: self.retry_count.load(Ordering::Acquire),
            reentry_count: self.reentry_count.load(Ordering::Acquire),
        });
        true
    }

    pub(crate) fn increment_retry_count(&self) -> u32 {
        self.retry_count.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(crate) fn increment_reentry_count(&self) -> u32 {
        self.reentry_count.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(crate) fn record_timer_wait(&self, mut wait: ToolLifecycleTimerWait) {
        let mut waits = self
            .timer_waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        wait.sequence = u32::try_from(waits.len() + 1).unwrap_or(u32::MAX);
        waits.push(wait);
    }

    pub(crate) fn deadline_after_ms(&self, timeout_ms: u64) -> Option<u64> {
        self.turn_timing
            .as_ref()
            .map(|timing| timing.monotonic_offset_ms().saturating_add(timeout_ms))
    }

    pub(crate) fn mark_first_poll(&self) {
        if self.first_poll_at.set(Instant::now()).is_ok()
            && let Some(turn_timing) = self.turn_timing.as_ref()
        {
            let _ = self.first_poll_at_ms.set(turn_timing.monotonic_offset_ms());
        }
    }

    pub(crate) fn mark_parallel_gate_admitted(&self) {
        let _ = self.parallel_gate_admitted_at.set(Instant::now());
        self.record_boundary(ToolLifecycleBoundary::Admitted);
    }

    pub(crate) fn record_authorization_state_coordination(&self, duration: Duration) {
        let _ = self.authorization_state_coordination.set(duration);
    }

    pub(crate) fn mark_handler_entry(&self) {
        if self.handler_entry_at.set(Instant::now()).is_err() {
            self.reentry_count.fetch_add(1, Ordering::AcqRel);
            return;
        }
        self.record_boundary(ToolLifecycleBoundary::HandlerStart);
    }

    pub(crate) fn mark_handler_exit(&self) {
        if self.handler_exit_at.set(Instant::now()).is_ok() {
            self.record_boundary(ToolLifecycleBoundary::HandlerReturn);
        }
    }

    pub(crate) fn mark_handler_exit_if_entered(&self) {
        if self.handler_entry_at.get().is_some() {
            self.mark_handler_exit();
        }
    }

    fn record_phase(target: &OnceLock<Duration>, duration: Duration) {
        let _ = target.set(duration);
    }

    pub(crate) fn record_workspace_evidence_before(&self, duration: Duration) {
        Self::record_phase(&self.workspace_evidence_before, duration);
    }

    pub(crate) fn record_workspace_evidence_before_attribution(
        &self,
        cache_hit: bool,
        timed_out_git_dependencies: Vec<String>,
    ) {
        let _ = self.workspace_evidence_before_cache_hit.set(cache_hit);
        let _ = self
            .workspace_evidence_before_timed_out_git_dependencies
            .set(timed_out_git_dependencies);
    }

    pub(crate) fn record_workspace_evidence_after(&self, duration: Duration) {
        Self::record_phase(&self.workspace_evidence_after, duration);
    }

    pub(crate) fn record_pre_tool_hook(&self, duration: Duration) {
        Self::record_phase(&self.pre_tool_hook, duration);
    }

    pub(crate) fn record_post_tool_hook(&self, duration: Duration) {
        Self::record_phase(&self.post_tool_hook, duration);
    }

    pub(crate) fn record_output_projection(&self, duration: Duration) {
        Self::record_phase(&self.output_projection, duration);
    }

    pub(crate) fn record_history_persistence(&self, duration: Duration) {
        Self::record_phase(&self.history_persistence, duration);
    }

    pub(crate) fn mark_output_collected(&self) {
        let _ = self.output_collected_at.set(Instant::now());
    }

    pub(crate) fn mark_exec_process_spawned(&self) {
        let _ = self.exec_process_spawned_at.set(Instant::now());
        self.record_boundary(ToolLifecycleBoundary::ProcessSpawn);
    }

    pub(crate) fn mark_exec_process_exited(&self) {
        if self.exec_process_exited_at.set(Instant::now()).is_ok() {
            self.record_boundary(ToolLifecycleBoundary::ProcessExit);
        }
    }

    pub(crate) fn mark_relay_enqueue(&self) -> bool {
        if self.has_boundary(ToolLifecycleBoundary::RelayEnqueue) {
            return false;
        }
        if let Some(turn_timing) = self.turn_timing.as_ref() {
            turn_timing.adjust_relay_queue_depth(1);
        }
        if self.record_boundary(ToolLifecycleBoundary::RelayEnqueue) {
            true
        } else {
            if let Some(turn_timing) = self.turn_timing.as_ref() {
                turn_timing.adjust_relay_queue_depth(-1);
            }
            false
        }
    }

    pub(crate) fn mark_relay_delivery(&self, execution_id: &ToolExecutionId) -> bool {
        if execution_id != &self.execution_id
            || !self.has_boundary(ToolLifecycleBoundary::RelayEnqueue)
            || self.has_boundary(ToolLifecycleBoundary::RelayDelivery)
        {
            return false;
        }
        if let Some(turn_timing) = self.turn_timing.as_ref() {
            turn_timing.adjust_relay_queue_depth(-1);
        }
        self.record_boundary(ToolLifecycleBoundary::RelayDelivery)
    }

    #[cfg(test)]
    pub(crate) fn mark_next_model_sample_start(&self) {
        self.record_boundary(ToolLifecycleBoundary::NextModelSampleStart);
    }

    fn has_boundary(&self, boundary: ToolLifecycleBoundary) -> bool {
        self.lifecycle_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|event| event.boundary == boundary)
    }

    pub(crate) fn record_outcome(&self, outcome: &'static str) {
        let _ = self.outcome.set(outcome);
    }

    pub(crate) fn record_exec_cleanup_state(
        &self,
        background_process_expected: bool,
        running_process_after_cleanup: bool,
    ) {
        self.exec_background_process_expected
            .store(background_process_expected, Ordering::Release);
        self.exec_running_process_after_cleanup
            .store(running_process_after_cleanup, Ordering::Release);
        self.exec_cleanup_state_recorded
            .store(true, Ordering::Release);
    }

    pub(crate) fn snapshot(&self, completed_at: Instant) -> ToolDispatchTimingSnapshot {
        let first_poll_at = self.first_poll_at.get().copied();
        let parallel_gate_admitted_at = self.parallel_gate_admitted_at.get().copied();
        let handler_entry_at = self.handler_entry_at.get().copied();
        let handler_exit_at = self.handler_exit_at.get().copied();
        let exec_process_spawned_at = self.exec_process_spawned_at.get().copied();
        let exec_process_exited_at = self.exec_process_exited_at.get().copied();
        let output_collected_at = self.output_collected_at.get().copied();
        ToolDispatchTimingSnapshot {
            execution_id: self.execution_id.clone(),
            sampling_generation_id: self.sampling_generation_id.clone(),
            lifecycle_events: self
                .lifecycle_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            timer_waits: self
                .timer_waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            retry_count: self.retry_count.load(Ordering::Acquire),
            reentry_count: self.reentry_count.load(Ordering::Acquire),
            outcome: self.outcome.get().copied(),
            first_poll_at_ms: self.first_poll_at_ms.get().copied(),
            item_to_first_poll_ms: first_poll_at
                .and_then(|at| duration_ms(at.saturating_duration_since(self.item_accepted_at))),
            parallel_gate_wait_ms: first_poll_at.and_then(|first_poll_at| {
                duration_ms(
                    parallel_gate_admitted_at
                        .unwrap_or(completed_at)
                        .saturating_duration_since(first_poll_at),
                )
            }),
            authorization_state_coordination_ms: self
                .authorization_state_coordination
                .get()
                .copied()
                .and_then(duration_ms),
            first_poll_to_handler_entry_ms: first_poll_at.zip(handler_entry_at).and_then(
                |(first_poll_at, handler_entry_at)| {
                    duration_ms(handler_entry_at.saturating_duration_since(first_poll_at))
                },
            ),
            handler_duration_ms: handler_entry_at.zip(handler_exit_at).and_then(
                |(handler_entry_at, handler_exit_at)| {
                    duration_ms(handler_exit_at.saturating_duration_since(handler_entry_at))
                },
            ),
            workspace_evidence_before_ms: self
                .workspace_evidence_before
                .get()
                .copied()
                .and_then(duration_ms),
            workspace_evidence_before_cache_hit: self
                .workspace_evidence_before_cache_hit
                .get()
                .copied(),
            workspace_evidence_before_timed_out_git_dependencies: self
                .workspace_evidence_before_timed_out_git_dependencies
                .get()
                .cloned()
                .unwrap_or_default(),
            workspace_evidence_after_ms: self
                .workspace_evidence_after
                .get()
                .copied()
                .and_then(duration_ms),
            pre_tool_hook_ms: self.pre_tool_hook.get().copied().and_then(duration_ms),
            post_tool_hook_ms: self.post_tool_hook.get().copied().and_then(duration_ms),
            output_projection_ms: self.output_projection.get().copied().and_then(duration_ms),
            history_persistence_ms: self
                .history_persistence
                .get()
                .copied()
                .and_then(duration_ms),
            first_poll_to_output_collected_ms: first_poll_at.zip(output_collected_at).and_then(
                |(first_poll_at, output_collected_at)| {
                    duration_ms(output_collected_at.saturating_duration_since(first_poll_at))
                },
            ),
            exec_request_to_spawn_ms: exec_process_spawned_at.and_then(|spawned_at| {
                duration_ms(spawned_at.saturating_duration_since(self.item_accepted_at))
            }),
            exec_spawn_to_exit_ms: exec_process_spawned_at
                .zip(exec_process_exited_at)
                .and_then(|(spawned_at, exited_at)| {
                    duration_ms(exited_at.saturating_duration_since(spawned_at))
                }),
            exec_exit_to_delivery_ms: exec_process_exited_at.and_then(|exited_at| {
                duration_ms(completed_at.saturating_duration_since(exited_at))
            }),
            exec_spawn_to_delivery_ms: exec_process_spawned_at.and_then(|spawned_at| {
                duration_ms(completed_at.saturating_duration_since(spawned_at))
            }),
            exec_process_alive_at_delivery: exec_process_spawned_at.is_some()
                && exec_process_exited_at.is_none(),
            exec_cleanup_state_observed: self.exec_cleanup_state_recorded.load(Ordering::Acquire),
            exec_background_process_expected: self
                .exec_cleanup_state_recorded
                .load(Ordering::Acquire)
                && self
                    .exec_background_process_expected
                    .load(Ordering::Acquire),
            exec_running_process_after_cleanup: self
                .exec_cleanup_state_recorded
                .load(Ordering::Acquire)
                && self
                    .exec_running_process_after_cleanup
                    .load(Ordering::Acquire),
            post_handler_ms: handler_exit_at
                .and_then(|at| duration_ms(completed_at.saturating_duration_since(at))),
            total_duration_ms: first_poll_at
                .and_then(|at| duration_ms(completed_at.saturating_duration_since(at))),
            parallel_gate_admitted: parallel_gate_admitted_at.is_some(),
            eager: self.eager,
        }
    }
}

fn duration_ms(duration: Duration) -> Option<u64> {
    u64::try_from(duration.as_millis()).ok()
}

pub(crate) async fn scope_tool_dispatch_timing<F>(
    timing: Arc<ToolDispatchTiming>,
    future: F,
) -> F::Output
where
    F: Future,
{
    ACTIVE_TOOL_DISPATCH_TIMING.scope(timing, future).await
}

pub(crate) fn record_authorization_state_coordination(duration: Duration) {
    let _ = ACTIVE_TOOL_DISPATCH_TIMING.try_with(|timing| {
        timing.record_authorization_state_coordination(duration);
    });
}

pub(crate) fn mark_tool_handler_entry() {
    let _ = ACTIVE_TOOL_DISPATCH_TIMING.try_with(|timing| timing.mark_handler_entry());
}

pub(crate) fn mark_tool_handler_exit() {
    let _ = ACTIVE_TOOL_DISPATCH_TIMING.try_with(|timing| timing.mark_handler_exit());
}

pub(crate) fn mark_exec_process_spawned() {
    let _ = ACTIVE_TOOL_DISPATCH_TIMING.try_with(|timing| timing.mark_exec_process_spawned());
}

pub(crate) fn mark_exec_process_exited() {
    let _ = ACTIVE_TOOL_DISPATCH_TIMING.try_with(|timing| timing.mark_exec_process_exited());
}

pub(crate) fn record_timer_wait(wait: ToolLifecycleTimerWait) {
    let _ = ACTIVE_TOOL_DISPATCH_TIMING.try_with(|timing| timing.record_timer_wait(wait));
}

pub(crate) fn lifecycle_deadline_after_ms(timeout_ms: u64) -> Option<u64> {
    ACTIVE_TOOL_DISPATCH_TIMING
        .try_with(|timing| timing.deadline_after_ms(timeout_ms))
        .ok()
        .flatten()
}

pub(crate) fn record_exec_cleanup_state(
    background_process_expected: bool,
    running_process_after_cleanup: bool,
) {
    let _ = ACTIVE_TOOL_DISPATCH_TIMING.try_with(|timing| {
        timing
            .record_exec_cleanup_state(background_process_expected, running_process_after_cleanup);
    });
}

pub(crate) fn active_tool_dispatch_timing() -> Option<Arc<ToolDispatchTiming>> {
    ACTIVE_TOOL_DISPATCH_TIMING.try_with(Arc::clone).ok()
}

macro_rules! record_phase {
    ($name:ident, $method:ident) => {
        pub(crate) fn $name(duration: Duration) {
            let _ = ACTIVE_TOOL_DISPATCH_TIMING.try_with(|timing| timing.$method(duration));
        }
    };
}

record_phase!(record_pre_tool_hook, record_pre_tool_hook);
record_phase!(record_post_tool_hook, record_post_tool_hook);
record_phase!(record_output_projection, record_output_projection);
record_phase!(record_history_persistence, record_history_persistence);
use crate::tools::context::ToolPayload;
use codex_rollout_trace::ExecutionStatus;
use codex_rollout_trace::ToolDispatchInvocation;
use codex_rollout_trace::ToolDispatchPayload;
use codex_rollout_trace::ToolDispatchRequester;
use codex_rollout_trace::ToolDispatchResult;
use codex_rollout_trace::ToolDispatchTraceContext;

/// Keeps registry early-return paths paired with trace end events.
pub(crate) struct ToolDispatchTrace {
    context: ToolDispatchTraceContext,
    terminal_tasks: tokio_util::task::TaskTracker,
}

impl ToolDispatchTrace {
    pub(crate) fn start(invocation: &ToolInvocation) -> Self {
        let context = invocation
            .session
            .services
            .rollout_thread_trace
            .start_tool_dispatch_trace(|| tool_dispatch_invocation(invocation));
        Self {
            context,
            terminal_tasks: invocation.session.terminal_tasks.clone(),
        }
    }

    pub(crate) fn record_completed(
        &self,
        invocation: &ToolInvocation,
        call_id: &str,
        payload: &ToolPayload,
        result: &dyn ToolOutput,
    ) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        if !self.context.is_enabled() {
            return Box::pin(async {});
        }

        let Some(result_payload) = tool_dispatch_result(invocation, call_id, payload, result)
        else {
            return Box::pin(async {});
        };
        let status = execution_status_for_outcome(result.outcome_context());
        let context = self.context.clone();
        let terminal_tasks = self.terminal_tasks.clone();
        Box::pin(async move {
            defer_trace_recording(&terminal_tasks, move || {
                context.record_completed(status, result_payload);
            })
            .await;
        })
    }

    pub(crate) fn record_failed(
        &self,
        error: &FunctionCallError,
    ) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        if !self.context.is_enabled() {
            return Box::pin(async {});
        }
        let context = self.context.clone();
        let error = error.to_string();
        let terminal_tasks = self.terminal_tasks.clone();
        Box::pin(async move {
            defer_trace_recording(&terminal_tasks, move || context.record_failed(error)).await;
        })
    }
}

async fn defer_trace_recording(
    terminal_tasks: &tokio_util::task::TaskTracker,
    record: impl FnOnce() + Send + 'static,
) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        if let Err(err) = terminal_tasks.spawn_blocking_on(record, &runtime).await {
            tracing::warn!("rollout trace recording task failed: {err}");
        }
    } else {
        record();
    }
}

fn execution_status_for_outcome(context: codex_tools::ToolOutputOutcomeContext) -> ExecutionStatus {
    match context.outcome {
        ToolOutputOutcome::Success => ExecutionStatus::Completed,
        ToolOutputOutcome::Failure | ToolOutputOutcome::TimedOut => ExecutionStatus::Failed,
        ToolOutputOutcome::Yielded => ExecutionStatus::Completed,
        ToolOutputOutcome::Skipped => {
            if context.skip_disposition
                == Some(ToolOutputSkipDisposition::BlockingRequiredOperation)
            {
                ExecutionStatus::Failed
            } else {
                ExecutionStatus::Cancelled
            }
        }
    }
}

fn tool_dispatch_invocation(invocation: &ToolInvocation) -> Option<ToolDispatchInvocation> {
    let requester = match &invocation.source {
        ToolCallSource::Direct => ToolDispatchRequester::Model {
            model_visible_call_id: invocation.call_id.clone(),
        },
        ToolCallSource::CodeMode {
            cell_id,
            runtime_tool_call_id,
            ..
        } => ToolDispatchRequester::CodeCell {
            runtime_cell_id: cell_id.clone(),
            runtime_tool_call_id: runtime_tool_call_id.clone(),
        },
    };

    Some(ToolDispatchInvocation {
        thread_id: invocation.session.thread_id.to_string(),
        codex_turn_id: invocation.step_context.turn.sub_id.clone(),
        tool_call_id: invocation.call_id.clone(),
        tool_name: invocation.tool_name.name.clone(),
        tool_namespace: invocation.tool_name.namespace.clone(),
        requester,
        payload: tool_dispatch_payload(&invocation.payload),
    })
}

fn tool_dispatch_result(
    invocation: &ToolInvocation,
    call_id: &str,
    payload: &ToolPayload,
    result: &dyn ToolOutput,
) -> Option<ToolDispatchResult> {
    match invocation.source {
        ToolCallSource::Direct => Some(ToolDispatchResult::DirectResponse {
            response_item: result.to_response_item(call_id, payload),
        }),
        ToolCallSource::CodeMode { .. } => Some(ToolDispatchResult::CodeModeResponse {
            value: result.code_mode_result(payload),
        }),
    }
}

fn tool_dispatch_payload(payload: &ToolPayload) -> ToolDispatchPayload {
    match payload {
        ToolPayload::Function { arguments } => ToolDispatchPayload::Function {
            arguments: arguments.clone(),
        },
        ToolPayload::ToolSearch { arguments } => ToolDispatchPayload::ToolSearch {
            arguments: arguments.clone(),
        },
        ToolPayload::Custom { input } => ToolDispatchPayload::Custom {
            input: input.clone(),
        },
    }
}

#[cfg(test)]
#[path = "tool_dispatch_trace_tests.rs"]
mod tests;
