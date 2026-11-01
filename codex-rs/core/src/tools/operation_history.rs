//! Capture and settlement adapters for the conversation-owned attempt journal.
use crate::FunctionCallError;
use crate::tools::context::{ToolCallSource, ToolInvocation};
use crate::tools::registry::AnyToolResult;
pub(crate) use codex_conversation::operation_history::{OperationAttempt, annotate_failure};
use codex_conversation::operation_history::{OperationAttemptInput, digest};

pub(crate) async fn capture(
        invocation: &ToolInvocation,
        contract_revision: Option<&str>,
        provider_authority: Option<&str>,
) -> OperationAttempt {
        let turn = &invocation.step_context.turn;
        // Release the tracker before taking session/history/persistence locks.
        let mutation = invocation.tracker.lock().await.current_mutation_revision();
        let source = match &invocation.source {
            ToolCallSource::Direct => serde_json::json!({"kind": "direct"}),
            ToolCallSource::CodeMode {
                cell_id,
                parent_call_id,
                runtime_tool_call_id,
                ..
            } => serde_json::json!({"kind": "code_mode", "cell_id": cell_id,
                    "parent_call_id": parent_call_id, "runtime_tool_call_id": runtime_tool_call_id}),
        };
        let backend = serde_json::json!({
            "provider_authority": provider_authority,
            "configured_environments": invocation.step_context.environments.turn_environments
                .iter().map(|environment| serde_json::json!({
                    "id": environment.environment_id,
                    "remote": environment.environment.is_remote(),
                })).collect::<Vec<_>>(),
            // The requested route is in the exact input identity. This layer
            // cannot certify which physical server handled an external call.
            "physical_backend": "unknown",
        });
        let parameters =
            crate::tools::registry::canonical_tool_invocation_value(&invocation.payload, None);
        let input_revision = parameters.as_ref().map(|value| digest(value.clone()));
        OperationAttempt::new(OperationAttemptInput {
            thread_id: invocation.session.thread_id.to_string(),
            turn_id: turn.sub_id.clone(),
            call_id: invocation.call_id.clone(),
            source,
            strategy: invocation.tool_name.clone(),
            operation: digest(
                serde_json::json!({"tool": invocation.tool_name, "backend": backend}),
            ),
            input_revision,
            effective_input_known: true,
            parameters,
            backend,
            configuration_revision: digest(serde_json::json!({
                "contract": contract_revision, "cwd": turn.cwd(),
                "permissions": turn.permission_profile(),
                "approval": turn.approval_policy.get(),
                "windows_sandbox": turn.windows_sandbox_level,
            })),
            // Counters are turn-local. Including their scope prevents false
            // equivalence after resume/fork or the next turn resets them.
            observed_state_revision: digest(serde_json::json!({
                "thread": invocation.session.thread_id.to_string(), "turn": turn.sub_id,
                "input": invocation.step_context.user_intent_epoch, "mutation": mutation,
            })),
        })
    }

pub(crate) fn settle(attempt: &mut OperationAttempt, result: &Result<AnyToolResult, FunctionCallError>) {
    match result {
        Ok(result) => settle_output(attempt, result),
        Err(error) => attempt.settle_outcome(codex_tools::ToolOutputOutcome::Failure, Some(error.failure())),
    }
}

pub(crate) fn settle_output(attempt: &mut OperationAttempt, result: &AnyToolResult) {
    attempt.settle_outcome(result.outcome_for_logging(), result.result.tool_failure());
}
