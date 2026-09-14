#![allow(dead_code)]

#[path = "../src/accounting.rs"]
mod accounting;

use accounting::GoalAccountingState;
use codex_protocol::config_types::ModeKind;
use codex_protocol::protocol::TokenUsage;
use pretty_assertions::assert_eq;

#[test]
fn goal_accounting_uses_turn_start_baseline_for_exact_deltas() {
    let state = GoalAccountingState::default();
    state.start_turn(
        "turn-1",
        ModeKind::Default,
        &token_usage(
            /*input_tokens*/ 100, /*cached_input_tokens*/ 10, /*output_tokens*/ 30,
            /*reasoning_output_tokens*/ 5, /*total_tokens*/ 135,
        ),
    );

    state.mark_turn_goal_active("turn-1", "goal-1");
    state.record_token_usage(
        "turn-1",
        &token_usage(
            /*input_tokens*/ 120, /*cached_input_tokens*/ 14, /*output_tokens*/ 42,
            /*reasoning_output_tokens*/ 8, /*total_tokens*/ 162,
        ),
    );
    assert_eq!(
        28,
        state
            .progress_snapshot("turn-1")
            .expect("active usage")
            .token_delta
    );
}

#[test]
fn goal_accounting_ignores_plan_mode_turns() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Plan, &TokenUsage::default());

    state.mark_turn_goal_active("turn-1", "goal-1");
    state.record_token_usage(
        "turn-1",
        &token_usage(
            /*input_tokens*/ 20, /*cached_input_tokens*/ 5, /*output_tokens*/ 8,
            /*reasoning_output_tokens*/ 2, /*total_tokens*/ 30,
        ),
    );

    assert!(state.progress_snapshot("turn-1").is_none());
}

#[test]
fn reasserting_the_same_goal_preserves_unflushed_tokens() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    state.mark_current_turn_goal_active("goal-1");
    state.record_token_usage("turn-1", &token_usage(20, 5, 8, 2, 30));
    state.mark_current_turn_goal_active("goal-1");
    assert_eq!(state.progress_snapshot("turn-1").unwrap().token_delta, 23);

    state.mark_current_turn_goal_active("goal-2");
    assert!(state.progress_snapshot("turn-1").is_none());
    state.record_token_usage("turn-1", &token_usage(25, 5, 10, 2, 37));
    assert_eq!(state.progress_snapshot("turn-1").unwrap().token_delta, 7);
}

#[test]
fn suspension_preserves_usage_and_ignores_later_notifications() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    state.mark_current_turn_goal_active("goal-1");
    state.record_token_usage("turn-1", &token_usage(20, 5, 8, 2, 30));
    state.suspend_accounting();
    state.record_token_usage("turn-1", &token_usage(100, 5, 80, 2, 180));
    assert_eq!(state.pending_turn_ids(), vec!["turn-1".to_string()]);
    assert_eq!(state.current_turn_id(), None);
    let snapshot = state.progress_snapshot("turn-1").unwrap();
    assert_eq!(snapshot.token_delta, 23);
    state.mark_progress_accounted_for_status(
        "turn-1",
        &snapshot,
        codex_state::ThreadGoalStatus::Paused,
        accounting::BudgetLimitedGoalDisposition::ClearActive,
    );
    assert!(state.progress_snapshot("turn-1").is_none());
}

#[test]
fn old_snapshot_does_not_clear_a_replacement_goal() {
    let state = GoalAccountingState::default();
    state.start_turn("turn-1", ModeKind::Default, &TokenUsage::default());
    state.mark_current_turn_goal_active("goal-1");
    state.record_token_usage("turn-1", &token_usage(20, 0, 0, 0, 20));
    let snapshot = state.progress_snapshot("turn-1").unwrap();
    state.mark_current_turn_goal_active("goal-2");
    state.record_token_usage("turn-1", &token_usage(25, 0, 0, 0, 25));
    state.mark_progress_accounted_for_status(
        "turn-1",
        &snapshot,
        codex_state::ThreadGoalStatus::Paused,
        accounting::BudgetLimitedGoalDisposition::ClearActive,
    );
    let remaining = state.progress_snapshot("turn-1").unwrap();
    assert_eq!(remaining.expected_goal_id, "goal-2");
    assert_eq!(remaining.token_delta, 5);
    assert!(state.has_active_goal());
}

fn token_usage(
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
    total_tokens: i64,
) -> TokenUsage {
    TokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
    }
}
