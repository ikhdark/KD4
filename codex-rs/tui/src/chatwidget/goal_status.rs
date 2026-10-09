//! Helpers for mapping thread-goal state into the compact status-line indicator.

use codex_app_server_protocol::ThreadGoal as AppThreadGoal;
use codex_app_server_protocol::ThreadGoalStatus as AppThreadGoalStatus;
use std::time::Instant;

use crate::bottom_pane::GoalStatusIndicator;
use crate::goal_display::format_goal_elapsed_seconds;
use crate::status::format_tokens_compact;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct GoalStatusState {
    goal: AppThreadGoal,
    observed_at: Instant,
}

impl GoalStatusState {
    pub(super) fn new(goal: AppThreadGoal, observed_at: Instant) -> Self {
        Self { goal, observed_at }
    }

    pub(super) fn is_active(&self) -> bool {
        self.goal.status == AppThreadGoalStatus::Active
    }

    pub(super) fn indicator(
        &self,
        now: Instant,
        active_turn_started_at: Option<Instant>,
    ) -> Option<GoalStatusIndicator> {
        let goal = &self.goal;
        if goal.status == AppThreadGoalStatus::Active
            && let Some(active_turn_started_at) = active_turn_started_at
        {
            let baseline = self.observed_at.max(active_turn_started_at);
            let active_seconds = now.saturating_duration_since(baseline).as_secs();
            let time_used_seconds = goal
                .time_used_seconds
                .saturating_add(i64::try_from(active_seconds).unwrap_or(i64::MAX));
            return Some(GoalStatusIndicator::Active {
                usage: active_goal_usage(goal.token_budget, goal.tokens_used, time_used_seconds),
            });
        }
        goal_status_indicator_from_app_goal(goal)
    }
}

pub(super) fn goal_status_indicator_from_app_goal(
    goal: &AppThreadGoal,
) -> Option<GoalStatusIndicator> {
    match goal.status {
        AppThreadGoalStatus::Active => Some(GoalStatusIndicator::Active {
            usage: active_goal_usage(goal.token_budget, goal.tokens_used, goal.time_used_seconds),
        }),
        AppThreadGoalStatus::Paused => Some(GoalStatusIndicator::Paused),
        AppThreadGoalStatus::Blocked => Some(GoalStatusIndicator::Blocked),
        AppThreadGoalStatus::UsageLimited => Some(GoalStatusIndicator::UsageLimited),
        AppThreadGoalStatus::BudgetLimited => Some(GoalStatusIndicator::BudgetLimited {
            usage: stopped_goal_budget_usage(goal.token_budget, goal.tokens_used),
        }),
        AppThreadGoalStatus::Complete => Some(GoalStatusIndicator::Complete {
            usage: Some(completed_goal_usage(
                goal.token_budget,
                goal.tokens_used,
                goal.time_used_seconds,
            )),
        }),
    }
}

fn active_goal_usage(
    token_budget: Option<i64>,
    tokens_used: i64,
    time_used_seconds: i64,
) -> Option<String> {
    if let Some(token_budget) = token_budget {
        return Some(format!(
            "{} / {}",
            format_tokens_compact(tokens_used),
            format_tokens_compact(token_budget)
        ));
    }

    Some(format_goal_elapsed_seconds(time_used_seconds))
}

fn stopped_goal_budget_usage(token_budget: Option<i64>, tokens_used: i64) -> Option<String> {
    token_budget.map(|token_budget| {
        format!(
            "{} / {} tokens",
            format_tokens_compact(tokens_used),
            format_tokens_compact(token_budget)
        )
    })
}

fn completed_goal_usage(
    token_budget: Option<i64>,
    tokens_used: i64,
    time_used_seconds: i64,
) -> String {
    if token_budget.is_some() {
        return format!("{} tokens", format_tokens_compact(tokens_used));
    }

    format_goal_elapsed_seconds(time_used_seconds)
}

#[cfg(test)]
mod tests {
    use super::GoalStatusState;
    use super::active_goal_usage;
    use super::completed_goal_usage;
    use super::stopped_goal_budget_usage;
    use crate::bottom_pane::GoalStatusIndicator;
    use codex_app_server_protocol::ThreadGoal as AppThreadGoal;
    use codex_app_server_protocol::ThreadGoalStatus as AppThreadGoalStatus;
    use std::time::Duration;
    use std::time::Instant;

    #[test]
    fn goal_usage_formats_budgeted_and_unbudgeted_states() {
        for (budget, active, stopped, complete) in [
            (Some(50_000), Some("12.5K / 50K"), Some("63.9K / 50K tokens"), "40K tokens"),
            (None, Some("2m"), None, "10h 12m"),
        ] {
            assert_eq!(
                active_goal_usage(budget, 12_500, 120),
                active.map(str::to_string)
            );
            assert_eq!(
                stopped_goal_budget_usage(budget, 63_876),
                stopped.map(str::to_string)
            );
            assert_eq!(completed_goal_usage(budget, 40_000, 36_720), complete);
        }
    }

    #[test]
    fn active_goal_status_counts_only_time_after_observation_and_turn_start() {
        let observed_at = Instant::now();
        let state = active_goal_state(observed_at, /*time_used_seconds*/ 60);
        for (now, started_at, expected) in [
            (observed_at + Duration::from_secs(60), Some(observed_at - Duration::from_secs(120)), "2m"),
            (observed_at + Duration::from_secs(180), Some(observed_at + Duration::from_secs(120)), "2m"),
            (observed_at, Some(observed_at + Duration::from_secs(120)), "1m"),
            (observed_at + Duration::from_secs(60), None, "1m"),
        ] {
            assert_eq!(
                state.indicator(now, started_at),
                Some(GoalStatusIndicator::Active {
                    usage: Some(expected.to_string())
                })
            );
        }
    }

    fn active_goal_state(observed_at: Instant, time_used_seconds: i64) -> GoalStatusState {
        GoalStatusState::new(
            AppThreadGoal {
                thread_id: "thread".to_string(),
                objective: "do the thing".to_string(),
                status: AppThreadGoalStatus::Active,
                token_budget: None,
                tokens_used: 0,
                time_used_seconds,
                created_at: 1,
                updated_at: 1,
            },
            observed_at,
        )
    }
}
