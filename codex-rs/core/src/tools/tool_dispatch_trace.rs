//! Adapter between core tool dispatch objects and rollout-trace events.
//!
//! `codex-rollout-trace` owns the event schema and writer behavior. This module
//! keeps the core-specific mapping from registry invocations/results out of the
//! registry control flow.

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolOutputProjection;
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

    pub(crate) fn record_completed(&self, success: bool, projection: &ToolOutputProjection) {
        if !self.context.is_enabled() {
            return;
        }

        let result_payload = tool_dispatch_result(projection);
        let status = if success {
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

fn tool_dispatch_result(projection: &ToolOutputProjection) -> ToolDispatchResult {
    match projection {
        ToolOutputProjection::Direct(response_item) => ToolDispatchResult::DirectResponse {
            response_item: response_item.clone(),
        },
        ToolOutputProjection::CodeMode(value) => ToolDispatchResult::CodeModeResponse {
            value: value.clone(),
        },
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
