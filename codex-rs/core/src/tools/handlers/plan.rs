use crate::FunctionCallError;
use crate::plan_store::PlanToolArgs;
use crate::plan_store::PlanToolResponse;
use crate::plan_store::PlanUpdateEffect;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::plan_spec::create_update_plan_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::EventMsg;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde_json::Value as JsonValue;
#[cfg(test)]
use std::collections::HashMap;
use std::sync::Arc;
#[cfg(test)]
use std::sync::LazyLock;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use tokio::sync::Notify;

pub struct PlanHandler;

pub struct PlanToolOutput {
    current_plan: UpdatePlanArgs,
    effect: PlanUpdateEffect,
    lineage: crate::plan_store::PlanLineage,
}

const PLAN_UPDATED_MESSAGE: &str = "Plan updated";
const PLAN_UNCHANGED_MESSAGE: &str = "Plan unchanged";

impl PlanToolOutput {
    fn response_result(&self) -> JsonValue {
        serde_json::json!(PlanToolResponse {
            completion_authority: crate::plan_store::checklist_completion_authority(),
            lineage: self.lineage.clone(),
            revision: crate::plan_store::plan_revision_with_lineage(Some(&self.current_plan), &self.lineage),
            step_ids: self.current_plan.plan.iter()
                .map(|item| self.lineage.step_id(&item.step)).collect(),
            message: self.message().to_string(),
            effect: self.effect.as_str().to_string(),
            no_progress: self.effect == PlanUpdateEffect::NoOp,
            current_plan: self.current_plan.clone(),
        })
    }

    fn message(&self) -> &'static str {
        match self.effect {
            PlanUpdateEffect::NoOp => PLAN_UNCHANGED_MESSAGE,
            PlanUpdateEffect::Initial
            | PlanUpdateEffect::StructuralRevision
            | PlanUpdateEffect::StatusOnly => PLAN_UPDATED_MESSAGE,
        }
    }
}

#[cfg(test)]
#[derive(Default)]
struct PlanCommitBoundaryHook {
    reached: Notify,
    cancellation_observed: Notify,
    release: Notify,
}

#[cfg(test)]
static PLAN_COMMIT_BOUNDARY_HOOKS: LazyLock<Mutex<HashMap<String, Arc<PlanCommitBoundaryHook>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
impl PlanCommitBoundaryHook {
    fn install(call_id: &str) -> Arc<Self> {
        let hook = Arc::new(Self::default());
        let previous = PLAN_COMMIT_BOUNDARY_HOOKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(call_id.to_string(), Arc::clone(&hook));
        assert!(
            previous.is_none(),
            "plan commit hook call IDs must be unique"
        );
        hook
    }

    async fn wait_until_reached(&self) {
        self.reached.notified().await;
    }

    fn release(&self) {
        self.release.notify_one();
    }
}

#[cfg(test)]
// Pauses scheduling only; the real runtime token observes its normal abort path.
async fn pause_at_plan_commit_boundary(
    call_id: &str,
    cancellation_token: &tokio_util::sync::CancellationToken,
) {
    let hook = PLAN_COMMIT_BOUNDARY_HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(call_id);
    if let Some(hook) = hook {
        hook.reached.notify_one();
        tokio::select! {
            _ = hook.release.notified() => {},
            _ = cancellation_token.cancelled() => {
                hook.cancellation_observed.notify_one();
                hook.release.notified().await;
            }
        }
    }
}

impl ToolOutput for PlanToolOutput {
    fn log_preview(&self) -> String {
        self.message().to_string()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn sampling_request_signal(&self) -> Option<JsonValue> {
        // Every effect carries the committed plan so the turn controller never
        // keeps a stale checklist; it counts only structural changes as revisions.
        Some(serde_json::json!({
            "kind": "plan_update",
            "plan": self.current_plan,
            "effect": self.effect.as_str(),
            "no_progress": self.effect == PlanUpdateEffect::NoOp,
        }))
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        let mut output = FunctionCallOutputPayload::from_text(self.response_result().to_string());
        output.success = Some(true);

        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output,
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.response_result()
    }
}

impl ToolExecutor<ToolInvocation> for PlanHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("update_plan")
    }

    fn spec(&self) -> ToolSpec {
        create_update_plan_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl PlanHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            cancellation_token,
            tracker: _,
            call_id: _call_id,
            source: _,
            payload,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "update_plan handler received unsupported payload".to_string(),
                ));
            }
        };

        if turn.collaboration_mode.mode == ModeKind::Plan {
            return Err(FunctionCallError::RespondToModel(
                "update_plan is a TODO/checklist tool and is not allowed in Plan mode".to_string(),
            ));
        }

        let mut requested_args = serde_json::from_str::<PlanToolArgs>(&arguments).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "failed to parse function arguments: {error}"
            ))
        })?;
        if requested_args.plan.is_some() == requested_args.set.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "provide exactly one of plan or set".to_string(),
            ));
        }
        if requested_args
            .plan
            .iter()
            .flatten()
            .filter(|item| item.status == codex_protocol::plan_tool::StepStatus::InProgress)
            .count()
            > 1
        {
            return Err(FunctionCallError::RespondToModel(
                "update_plan permits at most one in_progress step at a time".to_string(),
            ));
        }
        if cancellation_token.is_cancelled() {
            return Err(FunctionCallError::RespondToModel(
                "update_plan was cancelled before the plan update began".to_string(),
            ));
        }
        if let Some(workflow) = &requested_args.workflow {
            if workflow.len() > 128 {
                return Err(FunctionCallError::RespondToModel(
                    "workflow may link at most 128 existing assignments".into(),
                ));
            }
            let mut resolved = std::collections::BTreeMap::new();
            let coordinator = session.services.agent_control.task_coordinator();
            for (step_id, assignment_id) in workflow {
                let task = coordinator.get_agent_task(*assignment_id, Some(0)).await
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                if task.assignment.root_session_id != session.thread_id.to_string() {
                    return Err(FunctionCallError::RespondToModel(
                        "workflow assignments must belong to this root session".into(),
                    ));
                }
                resolved.insert(step_id.clone(), crate::plan_store::PlanExecutionNode {
                    assignment_id: *assignment_id,
                    dependencies: task.assignment.dependencies,
                    capability_profile: task.assignment.capability_profile,
                });
            }
            requested_args.resolved_workflow = Some(resolved);
        }
        #[cfg(test)]
        pause_at_plan_commit_boundary(&_call_id, &cancellation_token).await;

        let update = session
            .services
            .plan_store
            .update_tool(requested_args)
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        match update.effect {
            PlanUpdateEffect::Initial => turn.turn_timing_state.record_initial_plan_generation(),
            PlanUpdateEffect::StructuralRevision => {
                turn.turn_timing_state.record_plan_revision_generation()
            }
            PlanUpdateEffect::StatusOnly | PlanUpdateEffect::NoOp => {}
        }
        let output = PlanToolOutput {
            current_plan: update.current,
            effect: update.effect,
            lineage: update.lineage,
        };
        let response = output.response_result();
        // Even a no-op may retry an earlier failed publication or migrate a
        // legacy snapshot that had no requirement lineage.
        session
            .persist_rollout_items_durable(&[
                codex_protocol::protocol::RolloutItem::ResponseItem(
                    crate::plan_store::plan_snapshot_item(&response),
                ),
            ])
            .await
            .map_err(|error| FunctionCallError::RespondToModel(format!(
                "plan committed in memory but durable publication failed: {error}. Do not blindly replay this update. Committed state: {response}"
            )))?;
        session
            .send_event(turn.as_ref(), EventMsg::PlanUpdate(output.current_plan.clone()))
            .await;

        Ok(boxed_tool_output(output))
    }
}

impl CoreToolRuntime for PlanHandler {
    fn cancellation_recovery(
        &self,
        result: Option<&JsonValue>,
        error: Option<&str>,
    ) -> crate::tools::context::ToolEffectRecovery {
        match result.filter(|value| value.get("current_plan").is_some()) {
            Some(result) => crate::tools::context::ToolEffectRecovery::committed(result.clone()),
            None => crate::tools::context::ToolEffectRecovery::unknown(result, error),
        }
    }

    fn terminal_failure_reuse(&self) -> crate::tools::registry::TerminalFailureReuse {
        crate::tools::registry::TerminalFailureReuse::RequestRevisionAndJsonSyntax
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }

    fn cancellation_requires_commit_barrier(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
