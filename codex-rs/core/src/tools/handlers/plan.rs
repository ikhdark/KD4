use crate::function_tool::FunctionCallError;
use crate::task_evidence::PlanUpdateEffect;
use crate::task_evidence::PlanningTier;
use crate::task_evidence::PlanningUpdateInput;
use crate::task_evidence::ResultProvenance;
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
#[cfg(test)]
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
    normalized_plan: Option<UpdatePlanArgs>,
    effect: PlanUpdateEffect,
    normalization_reason: Option<String>,
    governor_plan: Option<UpdatePlanArgs>,
    unfinished_mutation_obligation: Option<bool>,
    validation_results: Vec<JsonValue>,
}

const PLAN_UPDATED_MESSAGE: &str = "Plan updated";
const PLAN_UNCHANGED_MESSAGE: &str = "Plan unchanged";

impl PlanToolOutput {
    fn response_result(&self) -> JsonValue {
        let mut result = serde_json::json!({
            "message": self.message(),
            "effect": self.effect.as_str(),
            "no_progress": self.effect == PlanUpdateEffect::NoOp,
            "current_plan": self.current_plan,
            "validation_results": self.validation_results,
        });
        if let Some(normalized_plan) = &self.normalized_plan {
            result["normalized_plan"] = serde_json::json!(normalized_plan);
        }
        if let Some(reason) = &self.normalization_reason {
            result["normalization_reason"] = JsonValue::String(reason.clone());
        }
        result
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
            "normalization_reason": self.normalization_reason,
            "unfinished_mutation_obligation": self.unfinished_mutation_obligation,
        }))
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        let text = self.response_result().to_string();
        let mut output = FunctionCallOutputPayload::from_text(text);
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
            turn,
            step_context: _,
            cancellation_token,
            tracker: _,
            call_id: _call_id,
            source: _,
            payload,
            ..
        } = invocation;

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

        let requested_input = parse_update_plan_arguments(&arguments)?;
        let repository = codex_git_utils::get_git_repo_root(turn.config.cwd.as_path())
            .unwrap_or_else(|| turn.config.cwd.to_path_buf());
        let routes = requested_input.validation_route.iter().chain(
            requested_input
                .plan
                .iter()
                .filter_map(|item| item.validation_route.as_ref()),
        );
        for route in routes {
            for leaf in &route.leaves {
                super::shell::validate_structured_validation_leaf(leaf, &repository).map_err(
                    |reason| {
                        FunctionCallError::RespondToModel(format!(
                            "structured validation route could not be bound: {reason}"
                        ))
                    },
                )?;
            }
        }
        if cancellation_token.is_cancelled() {
            return Err(FunctionCallError::RespondToModel(
                "update_plan was cancelled before the plan update began".to_string(),
            ));
        }
        #[cfg(test)]
        pause_at_plan_commit_boundary(&_call_id).await;
        let outcome = session
            .services
            .task_evidence
            .record_planning_update(requested_input.clone())
            .await;
        if !outcome.durably_recorded {
            return Err(FunctionCallError::RespondToModel(
                "update_plan could not be durably persisted; no plan update was acknowledged"
                    .to_string(),
            ));
        }
        match outcome.effect {
            PlanUpdateEffect::Initial => turn.turn_timing_state.record_initial_plan_generation(),
            PlanUpdateEffect::StructuralRevision => {
                turn.turn_timing_state.record_plan_revision_generation()
            }
            PlanUpdateEffect::StatusOnly | PlanUpdateEffect::NoOp => {}
        }
        let args = outcome.public_update;
        let requested_args = UpdatePlanArgs {
            explanation: requested_input.explanation,
            plan: requested_input.plan,
        };
        let validation_candidate = session
            .services
            .task_evidence
            .auto_validation_candidate()
            .await;
        let normalized_plan = (args != requested_args).then(|| args.clone());
        let normalization_reason = plan_normalization_reason(
            &requested_args,
            &args,
            outcome.effect,
            validation_candidate
                .as_ref()
                .map(|candidate| candidate.step_id.as_str()),
        );
        session
            .send_event(turn.as_ref(), EventMsg::PlanUpdate(args.clone()))
            .await;

        let validation_results = if let Some(candidate) = validation_candidate {
            let unsupported_runner = candidate.route.leaves.iter().find_map(|leaf| {
                super::shell::validate_structured_validation_leaf(leaf, &repository).err()
            });
            vec![serde_json::json!({
                "missing_proof": {
                    "step_id": candidate.step_id,
                    "implementation_identity": candidate.implementation_identity,
                },
                "stale_epoch": serde_json::Value::Null,
                "unsupported_runner": unsupported_runner,
                "validation_route": candidate.route,
                "runner_dispatch": "not_started",
            })]
        } else {
            Vec::new()
        };

        Ok(boxed_tool_output(PlanToolOutput {
            current_plan: args.clone(),
            normalized_plan,
            effect: outcome.effect,
            normalization_reason,
            governor_plan: outcome.effect.requests_generation().then_some(args),
            unfinished_mutation_obligation: outcome.unfinished_mutation_obligation,
            validation_results,
        }))
    }
}

fn plan_normalization_reason(
    requested: &UpdatePlanArgs,
    current: &UpdatePlanArgs,
    effect: PlanUpdateEffect,
    missing_proof_step: Option<&str>,
) -> Option<String> {
    if requested == current {
        return (effect == PlanUpdateEffect::NoOp)
            .then(|| "request matched the authoritative plan; no plan state changed".to_string());
    }
    let normalized_steps = requested
        .plan
        .iter()
        .filter_map(|requested_step| {
            current.plan.iter().find_map(|current_step| {
                let same_step = requested_step
                    .id
                    .as_ref()
                    .zip(current_step.id.as_ref())
                    .is_some_and(|(requested, current)| requested == current)
                    || requested_step.step == current_step.step;
                (same_step && requested_step.status != current_step.status).then(|| {
                    current_step
                        .id
                        .clone()
                        .unwrap_or_else(|| current_step.step.clone())
                })
            })
        })
        .collect::<Vec<_>>();
    Some(if let Some(step_id) = missing_proof_step {
        format!(
            "only the missing validation obligation [{step_id}] was normalized to in_progress; already proven steps were preserved"
        )
    } else if !normalized_steps.is_empty() {
        format!(
            "only unresolved obligation(s) [{}] were normalized; already proven steps were preserved",
            normalized_steps.join(", ")
        )
    } else {
        "the authoritative plan installed or preserved stable plan identifiers and structure"
            .to_string()
    })
}

impl CoreToolRuntime for PlanHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}

fn parse_update_plan_arguments(arguments: &str) -> Result<PlanningUpdateInput, FunctionCallError> {
    let input = serde_json::from_str::<PlanningUpdateInput>(arguments).map_err(|e| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {e}"))
    })?;
    if input.tier == Some(PlanningTier::Focused) && !input.plan.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "focused planning uses a WorkUnitId and cannot create PlanStep records".to_string(),
        ));
    }
    if input.tier == Some(PlanningTier::Focused) && input.mutation_obligations.len() > 1 {
        return Err(FunctionCallError::RespondToModel(
            "focused planning accepts at most one atomic mutation obligation".to_string(),
        ));
    }
    for fact in &input.facts {
        if fact.id.trim().is_empty()
            || fact.value.trim().is_empty()
            || fact
                .source
                .as_deref()
                .is_none_or(|source| source.trim().is_empty())
            || fact.provenance == ResultProvenance::Unverified
            || fact.depends_on_paths.is_empty()
            || fact
                .depends_on_paths
                .iter()
                .any(|path| path.trim().is_empty())
        {
            return Err(FunctionCallError::RespondToModel(
                "planning facts require a stable id, non-empty value, explicit provenance, concrete source locator, and at least one dependency path"
                    .to_string(),
            ));
        }
    }
    for removal in input.removed_facts.iter().chain(&input.removed_steps) {
        if removal.id.trim().is_empty() || removal.reason.trim().is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "planning removals require a stable id and non-empty reason".to_string(),
            ));
        }
    }
    Ok(input)
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
