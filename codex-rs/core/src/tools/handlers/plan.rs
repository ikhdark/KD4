use crate::FunctionCallError;
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
    governor_plan: Option<UpdatePlanArgs>,
}

const PLAN_UPDATED_MESSAGE: &str = "Plan updated";
const PLAN_UNCHANGED_MESSAGE: &str = "Plan unchanged";

impl PlanToolOutput {
    fn response_result(&self) -> JsonValue {
        serde_json::json!({
            "message": self.message(),
            "effect": self.effect.as_str(),
            "no_progress": self.effect == PlanUpdateEffect::NoOp,
            "current_plan": self.current_plan,
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
async fn pause_at_plan_commit_boundary(call_id: &str) {
    let hook = PLAN_COMMIT_BOUNDARY_HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(call_id);
    if let Some(hook) = hook {
        hook.reached.notify_one();
        hook.release.notified().await;
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
        Some(serde_json::json!({
            "kind": "plan_update",
            "plan": self.governor_plan,
            "effect": self.effect.as_str(),
            "no_progress": self.effect == PlanUpdateEffect::NoOp,
            "unfinished_mutation_obligation": false,
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

        let requested_args =
            serde_json::from_str::<UpdatePlanArgs>(&arguments).map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "failed to parse function arguments: {error}"
                ))
            })?;
        if cancellation_token.is_cancelled() {
            return Err(FunctionCallError::RespondToModel(
                "update_plan was cancelled before the plan update began".to_string(),
            ));
        }
        #[cfg(test)]
        pause_at_plan_commit_boundary(&_call_id).await;

        let update = session.services.plan_store.update(requested_args).await;
        match update.effect {
            PlanUpdateEffect::Initial => turn.turn_timing_state.record_initial_plan_generation(),
            PlanUpdateEffect::StructuralRevision => {
                turn.turn_timing_state.record_plan_revision_generation()
            }
            PlanUpdateEffect::StatusOnly | PlanUpdateEffect::NoOp => {}
        }
        session
            .send_event(turn.as_ref(), EventMsg::PlanUpdate(update.current.clone()))
            .await;

        Ok(boxed_tool_output(PlanToolOutput {
            governor_plan: update
                .effect
                .requests_generation()
                .then_some(update.current.clone()),
            current_plan: update.current,
            effect: update.effect,
        }))
    }
}

impl CoreToolRuntime for PlanHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
