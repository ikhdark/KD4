use codex_protocol::protocol::MAX_THREAD_GOAL_OBJECTIVE_CHARS;
use codex_protocol::protocol::ThreadGoal;
use codex_utils_template::Template;
use std::sync::LazyLock;

const MAX_RENDERED_GOAL_OBJECTIVE_BYTES: usize = MAX_THREAD_GOAL_OBJECTIVE_CHARS * "&amp;".len();
const GOAL_OBJECTIVE_TRUNCATED_MARKER: &str = "\n[objective truncated]";

static CONTINUATION_PROMPT_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    Template::parse_embedded(
        include_str!("../templates/goals/continuation.md"),
        "goals/continuation.md",
    )
});

static BUDGET_LIMIT_PROMPT_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    Template::parse_embedded(
        include_str!("../templates/goals/budget_limit.md"),
        "goals/budget_limit.md",
    )
});

static OBJECTIVE_UPDATED_PROMPT_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    Template::parse_embedded(
        include_str!("../templates/goals/objective_updated.md"),
        "goals/objective_updated.md",
    )
});

/// Builds the hidden prompt used to continue an active goal after the previous
/// turn completes.
pub fn continuation_prompt(goal: &ThreadGoal) -> String {
    let token_budget = goal
        .token_budget
        .map(|budget| budget.to_string())
        .unwrap_or_else(|| "unbounded".to_string());
    let remaining_tokens = goal
        .token_budget
        .map(|budget| (budget - goal.tokens_used).max(0).to_string())
        .unwrap_or_else(|| "unbounded".to_string());
    let tokens_used = goal.tokens_used.to_string();
    let objective = bounded_goal_objective(&goal.objective);

    match CONTINUATION_PROMPT_TEMPLATE.render([
        ("objective", objective.as_str()),
        ("tokens_used", tokens_used.as_str()),
        ("token_budget", token_budget.as_str()),
        ("remaining_tokens", remaining_tokens.as_str()),
    ]) {
        Ok(prompt) => prompt,
        Err(err) => panic!("embedded goals/continuation.md template failed to render: {err}"),
    }
}

/// Builds the hidden prompt used to ask the model to wrap up after a goal
/// exhausts its budget.
pub fn budget_limit_prompt(goal: &ThreadGoal) -> String {
    let token_budget = goal
        .token_budget
        .map(|budget| budget.to_string())
        .unwrap_or_else(|| "none".to_string());
    let tokens_used = goal.tokens_used.to_string();
    let time_used_seconds = goal.time_used_seconds.to_string();
    let objective = bounded_goal_objective(&goal.objective);

    match BUDGET_LIMIT_PROMPT_TEMPLATE.render([
        ("objective", objective.as_str()),
        ("tokens_used", tokens_used.as_str()),
        ("time_used_seconds", time_used_seconds.as_str()),
        ("token_budget", token_budget.as_str()),
    ]) {
        Ok(prompt) => prompt,
        Err(err) => panic!("embedded goals/budget_limit.md template failed to render: {err}"),
    }
}

/// Builds the hidden prompt used after a user edits an active goal.
pub fn objective_updated_prompt(goal: &ThreadGoal) -> String {
    let token_budget = goal
        .token_budget
        .map(|budget| budget.to_string())
        .unwrap_or_else(|| "unbounded".to_string());
    let remaining_tokens = goal
        .token_budget
        .map(|budget| (budget - goal.tokens_used).max(0).to_string())
        .unwrap_or_else(|| "unbounded".to_string());
    let tokens_used = goal.tokens_used.to_string();
    let objective = bounded_goal_objective(&goal.objective);

    match OBJECTIVE_UPDATED_PROMPT_TEMPLATE.render([
        ("objective", objective.as_str()),
        ("tokens_used", tokens_used.as_str()),
        ("token_budget", token_budget.as_str()),
        ("remaining_tokens", remaining_tokens.as_str()),
    ]) {
        Ok(prompt) => prompt,
        Err(err) => panic!("embedded goals/objective_updated.md template failed to render: {err}"),
    }
}

fn bounded_goal_objective(input: &str) -> String {
    let content_budget =
        MAX_RENDERED_GOAL_OBJECTIVE_BYTES.saturating_sub(GOAL_OBJECTIVE_TRUNCATED_MARKER.len());
    let mut bounded = String::with_capacity(input.len().min(MAX_RENDERED_GOAL_OBJECTIVE_BYTES));
    let mut truncation_boundary = 0;
    for character in input.chars() {
        let mut utf8 = [0; 4];
        let escaped_character = match character {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            _ => character.encode_utf8(&mut utf8),
        };
        if bounded.len() + escaped_character.len() > MAX_RENDERED_GOAL_OBJECTIVE_BYTES {
            bounded.truncate(truncation_boundary);
            bounded.truncate(bounded.trim_end().len());
            bounded.push_str(GOAL_OBJECTIVE_TRUNCATED_MARKER);
            return bounded;
        }
        bounded.push_str(escaped_character);
        // Reserve marker space only after actual overflow, at a whole escape boundary.
        if bounded.len() <= content_budget {
            truncation_boundary = bounded.len();
        }
    }
    bounded
}

#[cfg(test)]
#[path = "goals_tests.rs"]
mod goals_tests;
