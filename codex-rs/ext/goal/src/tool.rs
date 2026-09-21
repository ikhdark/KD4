use std::sync::Arc;

use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExposure;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::validate_thread_goal_objective;
use serde::Deserialize;
use serde::Serialize;

use crate::accounting::BudgetLimitedGoalDisposition;
use crate::accounting::GoalAccountingState;
use crate::analytics::GoalAnalytics;
use crate::analytics::GoalEventAttribution;
use crate::events::GoalEventEmitter;
use crate::metrics::GoalMetrics;
use crate::spec::CREATE_GOAL_TOOL_NAME;
use crate::spec::GET_GOAL_TOOL_NAME;
use crate::spec::UPDATE_GOAL_TOOL_NAME;
use crate::spec::create_create_goal_tool;
use crate::spec::create_get_goal_tool;
use crate::spec::create_update_goal_tool;

#[derive(Clone)]
pub(crate) struct GoalToolExecutor {
    kind: GoalToolKind,
    runtime: Arc<crate::runtime::GoalRuntimeHandle>,
    thread_id: ThreadId,
    state_db: Arc<codex_state::StateRuntime>,
    accounting_state: Arc<GoalAccountingState>,
    analytics: GoalAnalytics,
    event_emitter: GoalEventEmitter,
    metrics: GoalMetrics,
}

#[derive(Clone, Copy)]
enum GoalToolKind {
    Get,
    Create,
    Update,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateGoalRequest {
    pub objective: String,
    pub token_budget: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct UpdateGoalArgs {
    goal_ref: String,
    status: ThreadGoalStatus,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GoalToolResponse {
    goal_ref: Option<String>,
    accounting_pending: bool,
    goal: Option<ThreadGoal>,
    remaining_tokens: Option<i64>,
    completion_budget_report: Option<String>,
}

#[derive(Clone, Copy)]
enum CompletionBudgetReport {
    Include,
    Omit,
}

impl GoalToolExecutor {
    pub(crate) fn get(
        runtime: Arc<crate::runtime::GoalRuntimeHandle>,
        thread_id: ThreadId,
        state_db: Arc<codex_state::StateRuntime>,
        accounting_state: Arc<GoalAccountingState>,
        analytics: GoalAnalytics,
        event_emitter: GoalEventEmitter,
        metrics: GoalMetrics,
    ) -> Self {
        Self {
            kind: GoalToolKind::Get,
            runtime,
            thread_id,
            state_db,
            accounting_state,
            analytics,
            event_emitter,
            metrics,
        }
    }

    pub(crate) fn create(
        runtime: Arc<crate::runtime::GoalRuntimeHandle>,
        thread_id: ThreadId,
        state_db: Arc<codex_state::StateRuntime>,
        accounting_state: Arc<GoalAccountingState>,
        analytics: GoalAnalytics,
        event_emitter: GoalEventEmitter,
        metrics: GoalMetrics,
    ) -> Self {
        Self {
            kind: GoalToolKind::Create,
            runtime,
            thread_id,
            state_db,
            accounting_state,
            analytics,
            event_emitter,
            metrics,
        }
    }

    pub(crate) fn update(
        runtime: Arc<crate::runtime::GoalRuntimeHandle>,
        thread_id: ThreadId,
        state_db: Arc<codex_state::StateRuntime>,
        accounting_state: Arc<GoalAccountingState>,
        analytics: GoalAnalytics,
        event_emitter: GoalEventEmitter,
        metrics: GoalMetrics,
    ) -> Self {
        Self {
            kind: GoalToolKind::Update,
            runtime,
            thread_id,
            state_db,
            accounting_state,
            analytics,
            event_emitter,
            metrics,
        }
    }
}

impl ToolExecutor<ToolCall> for GoalToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(match self.kind {
            GoalToolKind::Get => GET_GOAL_TOOL_NAME,
            GoalToolKind::Create => CREATE_GOAL_TOOL_NAME,
            GoalToolKind::Update => UPDATE_GOAL_TOOL_NAME,
        })
    }

    fn spec(&self) -> ToolSpec {
        match self.kind {
            GoalToolKind::Get => create_get_goal_tool(),
            GoalToolKind::Create => create_create_goal_tool(),
            GoalToolKind::Update => create_update_goal_tool(),
        }
    }

    fn exposure(&self) -> ToolExposure {
        match (self.accounting_state.has_active_goal(), self.kind) {
            (true, GoalToolKind::Get | GoalToolKind::Update) => ToolExposure::Direct,
            _ => ToolExposure::Deferred,
        }
    }

    fn handle(&self, invocation: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let _goal_state_permit = tokio::select! {
                biased;
                _ = invocation.cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel("goal tool call cancelled".to_string())),
                permit = self.runtime.goal_state_permit() => permit.map_err(FunctionCallError::Fatal)?,
            };
            if invocation.cancellation_token.is_cancelled()
                || !self.runtime.admits_tool(&invocation.turn_id)
            {
                return Err(FunctionCallError::RespondToModel(
                    "goal tool call belongs to an inactive turn".to_string(),
                ));
            }
            match self.kind {
                GoalToolKind::Get => self.handle_get(invocation).await,
                GoalToolKind::Create => self.handle_create(invocation).await,
                GoalToolKind::Update => self.handle_update(invocation).await,
            }
        })
    }
}

impl GoalToolExecutor {
    async fn handle_get(
        &self,
        invocation: ToolCall,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let _ = invocation.function_arguments()?;
        self.account_active_goal_progress(
            &invocation.turn_id,
            codex_state::GoalAccountingMode::ActiveOnly,
            &invocation.call_id,
            BudgetLimitedGoalDisposition::KeepActive,
        )
        .await?;
        let goal = self
            .state_db
            .thread_goals()
            .get_thread_goal(self.thread_id)
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("failed to read goal: {err}"))
            })?;
        goal_response(goal, CompletionBudgetReport::Omit, false)
    }

    async fn handle_create(
        &self,
        invocation: ToolCall,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let mut request: CreateGoalRequest = parse_arguments(invocation.function_arguments()?)?;
        request.objective = request.objective.trim().to_string();
        validate_thread_goal_objective(&request.objective)
            .map_err(FunctionCallError::RespondToModel)?;
        validate_goal_budget(request.token_budget).map_err(FunctionCallError::RespondToModel)?;

        self.runtime
            .prepare_goal_creation(&invocation.turn_id)
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let goal = self
            .state_db
            .thread_goals()
            .insert_thread_goal(
                self.thread_id,
                request.objective.as_str(),
                codex_state::ThreadGoalStatus::Active,
                request.token_budget,
            )
            .await
            .map_err(|err| FunctionCallError::RespondToModel(format!("failed to create goal: {err}")))?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "cannot create a new goal because this thread has an unfinished goal; continue it or ask the user to clear or replace it. Do not mark it complete merely to replace it"
                        .to_string(),
                )
            })?;
        fill_empty_thread_preview_if_possible(self.state_db.as_ref(), self.thread_id, &goal).await;
        let turn_id = self
            .accounting_state
            .mark_current_turn_goal_active(goal.goal_id.clone());
        self.metrics.record_created();
        self.analytics.created(
            &goal,
            GoalEventAttribution::Turn(invocation.turn_id.as_str()),
        );
        self.emit_goal_updated_from_tool_call(
            &invocation,
            turn_id,
            protocol_goal_from_state(goal.clone()),
        );
        goal_response(Some(goal), CompletionBudgetReport::Omit, false)
    }

    async fn handle_update(
        &self,
        invocation: ToolCall,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let args: UpdateGoalArgs = parse_arguments(invocation.function_arguments()?)?;
        if !matches!(
            args.status,
            ThreadGoalStatus::Complete | ThreadGoalStatus::Blocked
        ) {
            return Err(FunctionCallError::RespondToModel(
                "update_goal can only mark the existing goal complete or blocked; pause, resume, budget-limited, and usage-limited status changes are controlled by the user or system"
                    .to_string(),
            ));
        }

        let intended_goal = self
            .state_db
            .thread_goals()
            .get_thread_goal(self.thread_id)
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("failed to read goal: {err}"))
            })?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "cannot update goal because this thread has no goal".to_string(),
                )
            })?;
        if args.goal_ref != goal_reference(&intended_goal) {
            return Err(FunctionCallError::RespondToModel(format!(
                "goal changed; reassess the current objective before updating: {}",
                serde_json::json!({"goal_ref": goal_reference(&intended_goal), "objective": intended_goal.objective})
            )));
        }
        self.runtime.suppress_goal(&intended_goal.goal_id);
        let accounting_result = self
            .account_active_goal_progress(
                &invocation.turn_id,
                match args.status {
                    ThreadGoalStatus::Complete => codex_state::GoalAccountingMode::ActiveOrComplete,
                    ThreadGoalStatus::Blocked => codex_state::GoalAccountingMode::ActiveOrStopped,
                    ThreadGoalStatus::Active
                    | ThreadGoalStatus::Paused
                    | ThreadGoalStatus::UsageLimited
                    | ThreadGoalStatus::BudgetLimited => unreachable!("status validated above"),
                },
                invocation.call_id.as_str(),
                BudgetLimitedGoalDisposition::ClearActive,
            )
            .await;
        let accounting_pending = accounting_result.is_err();
        if accounting_pending {
            self.accounting_state.suspend_accounting();
        }
        let previous_status = Some(intended_goal.status);
        let goal = self
            .state_db
            .thread_goals()
            .update_thread_goal(
                self.thread_id,
                codex_state::GoalUpdate {
                    objective: None,
                    status: Some(args.status),
                    token_budget: None,
                    expected_goal_id: Some(intended_goal.goal_id),
                },
            )
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("failed to update goal: {err}"))
            })?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "cannot update goal because this thread has no goal".to_string(),
                )
            })?;
        self.metrics
            .record_terminal_if_status_changed(previous_status, &goal);
        self.analytics.status_changed(
            &goal,
            previous_status,
            GoalEventAttribution::Turn(invocation.turn_id.as_str()),
        );
        let turn_id = if accounting_pending {
            Some(invocation.turn_id.clone())
        } else {
            self.accounting_state.clear_current_turn_goal()
        };
        self.emit_goal_updated_from_tool_call(
            &invocation,
            turn_id,
            protocol_goal_from_state(goal.clone()),
        );
        goal_response(
            Some(goal),
            if args.status == ThreadGoalStatus::Complete && !accounting_pending {
                CompletionBudgetReport::Include
            } else {
                CompletionBudgetReport::Omit
            },
            accounting_pending,
        )
    }

    fn emit_goal_updated_from_tool_call(
        &self,
        invocation: &ToolCall,
        turn_id: Option<String>,
        goal: ThreadGoal,
    ) {
        self.event_emitter
            .thread_goal_updated(invocation.call_id.clone(), turn_id, goal);
    }

    async fn account_active_goal_progress(
        &self,
        turn_id: &str,
        mode: codex_state::GoalAccountingMode,
        event_id: &str,
        budget_limited_goal_disposition: BudgetLimitedGoalDisposition,
    ) -> Result<Option<ThreadGoal>, FunctionCallError> {
        self.runtime
            .account_active_goal_progress(turn_id, event_id, mode, budget_limited_goal_disposition)
            .await
            .map(|progress| progress.map(|progress| progress.goal))
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "failed to account goal progress: {error}"
                ))
            })
    }
}

fn parse_arguments<T>(arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments)
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))
}

pub(crate) fn validate_goal_budget(value: Option<i64>) -> Result<(), String> {
    if let Some(value) = value
        && value <= 0
    {
        return Err("goal budgets must be positive when provided".to_string());
    }
    Ok(())
}

fn goal_response(
    goal: Option<codex_state::ThreadGoal>,
    completion_budget_report: CompletionBudgetReport,
    accounting_pending: bool,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let value = serde_json::to_value(GoalToolResponse::new(
        goal,
        completion_budget_report,
        accounting_pending,
    ))
    .map_err(|err| FunctionCallError::Fatal(err.to_string()))?;
    Ok(Box::new(JsonToolOutput::new(value)))
}

impl GoalToolResponse {
    fn new(
        goal: Option<codex_state::ThreadGoal>,
        report_mode: CompletionBudgetReport,
        accounting_pending: bool,
    ) -> Self {
        let goal_ref = goal.as_ref().map(goal_reference);
        let goal = goal.map(protocol_goal_from_state);
        let remaining_tokens = goal.as_ref().and_then(|goal| {
            goal.token_budget
                .map(|budget| (budget - goal.tokens_used).max(0))
        });
        let completion_budget_report = match report_mode {
            CompletionBudgetReport::Include => goal
                .as_ref()
                .filter(|goal| goal.status == ThreadGoalStatus::Complete)
                .and_then(completion_budget_report),
            CompletionBudgetReport::Omit => None,
        };
        Self {
            goal_ref,
            accounting_pending,
            goal,
            remaining_tokens,
            completion_budget_report,
        }
    }
}

pub(crate) async fn fill_empty_thread_preview_if_possible(
    state_db: &codex_state::StateRuntime,
    thread_id: ThreadId,
    goal: &codex_state::ThreadGoal,
) {
    if let Err(err) = state_db
        .set_thread_preview_if_empty(thread_id, goal.objective.as_str())
        .await
    {
        tracing::warn!(
            "failed to set empty thread preview from goal objective for {thread_id}: {err}"
        );
    }
}

pub(crate) fn protocol_goal_from_state(goal: codex_state::ThreadGoal) -> ThreadGoal {
    ThreadGoal {
        thread_id: goal.thread_id,
        objective: goal.objective,
        status: goal.status,
        token_budget: goal.token_budget,
        tokens_used: goal.tokens_used,
        time_used_seconds: goal.time_used_seconds,
        created_at: goal.created_at.timestamp(),
        updated_at: goal.updated_at.timestamp(),
    }
}

fn completion_budget_report(goal: &ThreadGoal) -> Option<String> {
    if goal.token_budget.is_none() && goal.time_used_seconds <= 0 {
        None
    } else {
        Some(
            "Goal achieved. Report final usage from this tool result's structured goal fields. If `goal.tokenBudget` is present, include token usage from `goal.tokensUsed` and `goal.tokenBudget`. If `goal.timeUsedSeconds` is greater than 0, summarize elapsed time in a concise, human-friendly form appropriate to the response language."
                .to_string(),
        )
    }
}

/// Stable across reloads and unaffected by usage updates; binds completion to intent.
pub(crate) fn goal_reference(goal: &codex_state::ThreadGoal) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(goal.objective.as_bytes());
    format!("{}:{digest:x}", goal.goal_id)
}
