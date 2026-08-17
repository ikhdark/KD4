use crate::function_tool::FunctionCallError;
use crate::task_evidence::PlanUpdateEffect;
use crate::task_evidence::PlanningTier;
use crate::task_evidence::PlanningUpdateInput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolCallSource;
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
use codex_protocol::plan_tool::ValidationRouteLeaf;
use codex_protocol::plan_tool::ValidationRouteOrdering;
use codex_protocol::protocol::EventMsg;
use codex_protocol::validation::ValidationTerminalStatus;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_tools::shell_command_backend_for_features;
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
    normalized_plan: Option<UpdatePlanArgs>,
    governor_plan: Option<UpdatePlanArgs>,
    unfinished_mutation_obligation: Option<bool>,
    validation_results: Vec<JsonValue>,
}

const PLAN_UPDATED_MESSAGE: &str = "Plan updated";

impl PlanToolOutput {
    fn normalized_result(&self) -> Option<JsonValue> {
        self.normalized_plan
            .as_ref()
            .map(|normalized_plan| {
                serde_json::json!({
                    "message": PLAN_UPDATED_MESSAGE,
                    "normalized_plan": normalized_plan,
                    "validation_results": self.validation_results,
                })
            })
            .or_else(|| {
                (!self.validation_results.is_empty()).then(|| {
                    serde_json::json!({
                        "message": PLAN_UPDATED_MESSAGE,
                        "validation_results": self.validation_results,
                    })
                })
            })
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
        PLAN_UPDATED_MESSAGE.to_string()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn sampling_request_signal(&self) -> Option<JsonValue> {
        Some(serde_json::json!({
            "kind": "plan_update",
            "plan": self.governor_plan,
            "unfinished_mutation_obligation": self.unfinished_mutation_obligation,
        }))
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        let text = self.normalized_result().map_or_else(
            || PLAN_UPDATED_MESSAGE.to_string(),
            |result| result.to_string(),
        );
        let mut output = FunctionCallOutputPayload::from_text(text);
        output.success = Some(true);

        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output,
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.normalized_result()
            .unwrap_or_else(|| JsonValue::Object(serde_json::Map::new()))
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
            step_context,
            cancellation_token,
            tracker,
            call_id: _call_id,
            source,
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
        let normalized_plan = (args != requested_args).then(|| args.clone());
        session
            .send_event(turn.as_ref(), EventMsg::PlanUpdate(args.clone()))
            .await;

        let validation_results = if let Some(candidate) = session
            .services
            .task_evidence
            .auto_validation_candidate()
            .await
        {
            run_bound_validation_route(
                session.clone(),
                turn.clone(),
                step_context,
                tracker,
                cancellation_token.clone(),
                source,
                &_call_id,
                candidate,
            )
            .await
        } else {
            Vec::new()
        };

        Ok(boxed_tool_output(PlanToolOutput {
            normalized_plan,
            governor_plan: outcome.effect.requests_generation().then_some(args),
            unfinished_mutation_obligation: outcome.unfinished_mutation_obligation,
            validation_results,
        }))
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_bound_validation_route(
    session: Arc<crate::session::session::Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
    step_context: Arc<crate::session::step_context::StepContext>,
    tracker: SharedTurnDiffTracker,
    cancellation_token: tokio_util::sync::CancellationToken,
    source: ToolCallSource,
    parent_call_id: &str,
    candidate: crate::task_evidence::AutoValidationCandidate,
) -> Vec<JsonValue> {
    let repository = turn.config.cwd.to_path_buf();
    let repository = codex_git_utils::get_git_repo_root(&repository).unwrap_or(repository);

    let mut results = Vec::with_capacity(candidate.route.leaves.len());
    let route_started_at = std::time::Instant::now();
    if candidate.route.ordering == ValidationRouteOrdering::RunAll {
        let executions = candidate
            .route
            .leaves
            .iter()
            .enumerate()
            .map(|(index, leaf)| {
                run_bound_validation_leaf(
                    session.clone(),
                    turn.clone(),
                    step_context.clone(),
                    tracker.clone(),
                    cancellation_token.clone(),
                    source.clone(),
                    parent_call_id,
                    &repository,
                    &candidate,
                    index,
                    leaf,
                )
            });
        results.extend(
            futures::future::join_all(executions)
                .await
                .into_iter()
                .flatten(),
        );
    } else {
        for (index, leaf) in candidate.route.leaves.iter().enumerate() {
            let Some(result) = run_bound_validation_leaf(
                session.clone(),
                turn.clone(),
                step_context.clone(),
                tracker.clone(),
                cancellation_token.clone(),
                source.clone(),
                parent_call_id,
                &repository,
                &candidate,
                index,
                leaf,
            )
            .await
            else {
                break;
            };
            let success = super::shell::ValidationExecutionOutcome::from_value(&result)
                == Some(super::shell::ValidationExecutionOutcome::ExecutedSuccess);
            results.push(result);
            if !success {
                break;
            }
        }
    }
    if !results.is_empty() {
        let aggregate_outcome = if results.iter().any(|result| {
            super::shell::ValidationExecutionOutcome::from_value(result)
                == Some(super::shell::ValidationExecutionOutcome::ExecutedFailure)
        }) {
            super::shell::ValidationExecutionOutcome::ExecutedFailure
        } else if results.iter().all(|result| {
            super::shell::ValidationExecutionOutcome::from_value(result)
                == Some(super::shell::ValidationExecutionOutcome::ExecutedSuccess)
        }) {
            super::shell::ValidationExecutionOutcome::ExecutedSuccess
        } else {
            super::shell::ValidationExecutionOutcome::NotExecuted
        };
        let completed_leaf_count = results.len();
        results.push(serde_json::json!({
            "step_id": candidate.step_id,
            "aggregate": true,
            "ordering": candidate.route.ordering,
            "declared_leaf_count": candidate.route.leaves.len(),
            "completed_leaf_count": completed_leaf_count,
            "success": aggregate_outcome.success(),
            "execution_outcome": aggregate_outcome.as_str(),
            "command_was_executed": aggregate_outcome != super::shell::ValidationExecutionOutcome::NotExecuted,
            "duration_ms": u64::try_from(route_started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        }));
    }
    results
}

#[allow(clippy::too_many_arguments)]
async fn run_bound_validation_leaf(
    session: Arc<crate::session::session::Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
    step_context: Arc<crate::session::step_context::StepContext>,
    tracker: SharedTurnDiffTracker,
    cancellation_token: tokio_util::sync::CancellationToken,
    source: ToolCallSource,
    parent_call_id: &str,
    repository: &std::path::Path,
    candidate: &crate::task_evidence::AutoValidationCandidate,
    index: usize,
    leaf: &ValidationRouteLeaf,
) -> Option<JsonValue> {
    if cancellation_token.is_cancelled() {
        return None;
    }
    // Re-read the exact route immediately before every start/join. A newer
    // edit or plan change supersedes the pending launch without rediscovery.
    let current = session
        .services
        .task_evidence
        .auto_validation_candidate()
        .await;
    if current.as_ref().is_none_or(|current| {
        current.step_id != candidate.step_id
            || current.implementation_identity != candidate.implementation_identity
            || current.route != candidate.route
    }) {
        tracing::info!(step_id = %candidate.step_id, "auto-validation launch superseded");
        return None;
    }
    if let Err(reason) = super::shell::validate_structured_validation_leaf(leaf, repository) {
        tracing::info!(%reason, step_id = %candidate.step_id, "auto-validation fresh admission rejected");
        return None;
    }
    let (program, argv) = leaf.argv.split_first()?;
    let synthetic_call_id = format!("{parent_call_id}:validation:{index}");
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "argv",
            "program": program,
            "args": argv,
            "timeout_ms": leaf.timeout_ms,
            "workdir": repository,
        })
        .to_string(),
    };
    let started_at = std::time::Instant::now();
    let invocation = ToolInvocation {
        session: session.clone(),
        turn: turn.clone(),
        step_context: step_context.clone(),
        cancellation_token: cancellation_token.clone(),
        tracker: tracker.clone(),
        call_id: synthetic_call_id.clone(),
        tool_name: ToolName::plain("shell_command"),
        source: source.clone(),
        payload: payload.clone(),
    };
    let handler =
        super::shell::ShellCommandHandler::new(super::shell::ShellCommandHandlerOptions {
            backend_config: shell_command_backend_for_features(turn.config.features.get()),
            allow_login_shell: false,
            exec_permission_approvals_enabled: false,
        });
    let bound = crate::tools::command_execution::BoundAutoValidationLeaf {
        step_id: candidate.step_id.clone(),
        implementation_revision: candidate.implementation_revision,
        implementation_identity: candidate.implementation_identity.clone(),
        repository: repository.to_path_buf(),
        route: candidate.route.clone(),
        leaf_index: index,
    };
    if !session
        .services
        .command_execution
        .bind_auto_validation_leaf(synthetic_call_id.clone(), bound)
        .await
    {
        tracing::warn!(%synthetic_call_id, "duplicate auto-validation call binding");
        return None;
    }
    let result = handler.handle_call(invocation).await;
    session
        .services
        .command_execution
        .clear_auto_validation_leaf(&synthetic_call_id)
        .await;
    let current = session
        .services
        .task_evidence
        .auto_validation_candidate()
        .await;
    let superseded = current.as_ref().is_none_or(|current| {
        current.step_id != candidate.step_id
            || current.implementation_identity != candidate.implementation_identity
            || current.route != candidate.route
    });
    let settled = if superseded {
        session
            .services
            .command_execution
            .supersede_validation_result_for_call(&synthetic_call_id)
            .await
    } else {
        session
            .services
            .command_execution
            .validation_result_for_call(&synthetic_call_id)
            .await
    };
    let (execution_outcome, output) = match result {
        Ok(output) => {
            if let Some(settled) = settled {
                let execution_outcome = match settled.status {
                    ValidationTerminalStatus::Succeeded => {
                        super::shell::ValidationExecutionOutcome::ExecutedSuccess
                    }
                    ValidationTerminalStatus::Superseded => {
                        super::shell::ValidationExecutionOutcome::NotExecuted
                    }
                    ValidationTerminalStatus::Failed => {
                        super::shell::ValidationExecutionOutcome::ExecutedFailure
                    }
                };
                (
                    execution_outcome,
                    serde_json::json!({ "validation_result": settled }),
                )
            } else {
                let execution_outcome = match output.outcome_context().outcome {
                    codex_tools::ToolOutputOutcome::Success => {
                        super::shell::ValidationExecutionOutcome::ExecutedSuccess
                    }
                    codex_tools::ToolOutputOutcome::Failure
                    | codex_tools::ToolOutputOutcome::TimedOut => {
                        super::shell::ValidationExecutionOutcome::ExecutedFailure
                    }
                    codex_tools::ToolOutputOutcome::Skipped => {
                        super::shell::ValidationExecutionOutcome::NotExecuted
                    }
                };
                (execution_outcome, output.code_mode_result(&payload))
            }
        }
        Err(error) => (
            super::shell::ValidationExecutionOutcome::ExecutedFailure,
            serde_json::json!({ "error": error.to_string() }),
        ),
    };
    Some(serde_json::json!({
        "step_id": candidate.step_id,
        "leaf_index": index,
        "call_id": synthetic_call_id,
        "freshness": "executed_or_shared",
        "success": execution_outcome.success(),
        "execution_outcome": execution_outcome.as_str(),
        "command_was_executed": execution_outcome != super::shell::ValidationExecutionOutcome::NotExecuted,
        "duration_ms": u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        "output": output,
    }))
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
