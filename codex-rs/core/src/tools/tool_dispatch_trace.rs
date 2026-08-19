//! Adapter between core tool dispatch objects and rollout-trace events.
//!
//! `codex-rollout-trace` owns the event schema and writer behavior. This module
//! keeps the core-specific mapping from registry invocations/results out of the
//! registry control flow.

use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::time::Instant;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputSkipDisposition;

tokio::task_local! {
    static ACTIVE_TOOL_DISPATCH_TIMING: Arc<ToolDispatchTiming>;
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolDispatchTimingSnapshot {
    pub item_to_first_poll_ms: Option<u64>,
    pub parallel_gate_wait_ms: Option<u64>,
    pub authorization_state_coordination_ms: Option<u64>,
    pub first_poll_to_handler_entry_ms: Option<u64>,
    pub handler_duration_ms: Option<u64>,
    pub workspace_evidence_before_ms: Option<u64>,
    pub workspace_evidence_after_ms: Option<u64>,
    pub pre_tool_hook_ms: Option<u64>,
    pub post_tool_hook_ms: Option<u64>,
    pub output_projection_ms: Option<u64>,
    pub history_persistence_ms: Option<u64>,
    pub post_handler_ms: Option<u64>,
    pub total_duration_ms: Option<u64>,
    pub parallel_gate_admitted: bool,
    pub eager: bool,
}

#[derive(Debug)]
pub(crate) struct ToolDispatchTiming {
    item_accepted_at: Instant,
    first_poll_at: OnceLock<Instant>,
    parallel_gate_admitted_at: OnceLock<Instant>,
    authorization_state_coordination: OnceLock<Duration>,
    handler_entry_at: OnceLock<Instant>,
    handler_exit_at: OnceLock<Instant>,
    workspace_evidence_before: OnceLock<Duration>,
    workspace_evidence_after: OnceLock<Duration>,
    pre_tool_hook: OnceLock<Duration>,
    post_tool_hook: OnceLock<Duration>,
    output_projection: OnceLock<Duration>,
    history_persistence: OnceLock<Duration>,
    eager: bool,
}

impl ToolDispatchTiming {
    pub(crate) fn new(item_accepted_at: Instant, eager: bool) -> Self {
        Self {
            item_accepted_at,
            first_poll_at: OnceLock::new(),
            parallel_gate_admitted_at: OnceLock::new(),
            authorization_state_coordination: OnceLock::new(),
            handler_entry_at: OnceLock::new(),
            handler_exit_at: OnceLock::new(),
            workspace_evidence_before: OnceLock::new(),
            workspace_evidence_after: OnceLock::new(),
            pre_tool_hook: OnceLock::new(),
            post_tool_hook: OnceLock::new(),
            output_projection: OnceLock::new(),
            history_persistence: OnceLock::new(),
            eager,
        }
    }

    pub(crate) fn mark_first_poll(&self) {
        let _ = self.first_poll_at.set(Instant::now());
    }

    pub(crate) fn mark_parallel_gate_admitted(&self) {
        let _ = self.parallel_gate_admitted_at.set(Instant::now());
    }

    pub(crate) fn record_authorization_state_coordination(&self, duration: Duration) {
        let _ = self.authorization_state_coordination.set(duration);
    }

    pub(crate) fn mark_handler_entry(&self) {
        let _ = self.handler_entry_at.set(Instant::now());
    }

    pub(crate) fn mark_handler_exit(&self) {
        let _ = self.handler_exit_at.set(Instant::now());
    }

    fn record_phase(target: &OnceLock<Duration>, duration: Duration) {
        let _ = target.set(duration);
    }

    pub(crate) fn record_workspace_evidence_before(&self, duration: Duration) {
        Self::record_phase(&self.workspace_evidence_before, duration);
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

    pub(crate) fn snapshot(&self, completed_at: Instant) -> ToolDispatchTimingSnapshot {
        let first_poll_at = self.first_poll_at.get().copied();
        let parallel_gate_admitted_at = self.parallel_gate_admitted_at.get().copied();
        let handler_entry_at = self.handler_entry_at.get().copied();
        let handler_exit_at = self.handler_exit_at.get().copied();
        ToolDispatchTimingSnapshot {
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

macro_rules! record_phase {
    ($name:ident, $method:ident) => {
        pub(crate) fn $name(duration: Duration) {
            let _ = ACTIVE_TOOL_DISPATCH_TIMING.try_with(|timing| timing.$method(duration));
        }
    };
}

record_phase!(
    record_workspace_evidence_before,
    record_workspace_evidence_before
);
record_phase!(
    record_workspace_evidence_after,
    record_workspace_evidence_after
);
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
}

impl ToolDispatchTrace {
    pub(crate) fn start(invocation: &ToolInvocation) -> Self {
        let context = invocation
            .session
            .services
            .rollout_thread_trace
            .start_tool_dispatch_trace(|| tool_dispatch_invocation(invocation));
        Self { context }
    }

    pub(crate) fn record_completed(
        &self,
        invocation: &ToolInvocation,
        call_id: &str,
        payload: &ToolPayload,
        result: &dyn ToolOutput,
    ) {
        if !self.context.is_enabled() {
            return;
        }

        let Some(result_payload) = tool_dispatch_result(invocation, call_id, payload, result)
        else {
            return;
        };
        let status = execution_status_for_outcome(result.outcome_context());
        let context = self.context.clone();
        defer_trace_recording(move || context.record_completed(status, result_payload));
    }

    pub(crate) fn record_failed(&self, error: &FunctionCallError) {
        if !self.context.is_enabled() {
            return;
        }
        let context = self.context.clone();
        let error = error.to_string();
        defer_trace_recording(move || context.record_failed(error));
    }
}

fn defer_trace_recording(record: impl FnOnce() + Send + 'static) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        drop(runtime.spawn_blocking(record));
    } else {
        record();
    }
}

fn execution_status_for_outcome(context: codex_tools::ToolOutputOutcomeContext) -> ExecutionStatus {
    match context.outcome {
        ToolOutputOutcome::Success => ExecutionStatus::Completed,
        ToolOutputOutcome::Failure | ToolOutputOutcome::TimedOut => ExecutionStatus::Failed,
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
        } => ToolDispatchRequester::CodeCell {
            runtime_cell_id: cell_id.clone(),
            runtime_tool_call_id: runtime_tool_call_id.clone(),
        },
    };

    Some(ToolDispatchInvocation {
        thread_id: invocation.session.thread_id.to_string(),
        codex_turn_id: invocation.turn.sub_id.clone(),
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
