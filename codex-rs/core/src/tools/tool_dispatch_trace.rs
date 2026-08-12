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

    pub(crate) fn snapshot(&self, completed_at: Instant) -> ToolDispatchTimingSnapshot {
        let first_poll_at = self.first_poll_at.get().copied();
        let parallel_gate_admitted_at = self.parallel_gate_admitted_at.get().copied();
        let handler_entry_at = self.handler_entry_at.get().copied();
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
            // Preserve the established event meanings: handler duration begins at
            // gate admission, and total duration begins at the first dispatch poll.
            handler_duration_ms: parallel_gate_admitted_at.and_then(|admitted_at| {
                duration_ms(completed_at.saturating_duration_since(admitted_at))
            }),
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
        let status = if result.success_for_logging() {
            ExecutionStatus::Completed
        } else {
            ExecutionStatus::Failed
        };
        self.context.record_completed(status, result_payload);
    }

    pub(crate) fn record_failed(&self, error: &FunctionCallError) {
        self.context.record_failed(error);
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
