use codex_core::context::ContextualUserFragment;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
use codex_prompts::budget_limit_prompt;
use codex_prompts::continuation_prompt;
use codex_prompts::objective_updated_prompt;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadGoal;

pub(crate) fn budget_limit_steering_item(goal: &ThreadGoal) -> ResponseItem {
    goal_context_input_item(budget_limit_prompt(goal))
}

pub(crate) fn objective_updated_steering_item(goal: &ThreadGoal) -> ResponseItem {
    goal_context_input_item(objective_updated_prompt(goal))
}

pub(crate) fn continuation_steering_item(goal: &ThreadGoal) -> ResponseItem {
    goal_context_input_item(continuation_prompt(goal))
}

fn goal_context_input_item(prompt: String) -> ResponseItem {
    ContextualUserFragment::into(InternalModelContextFragment::new(
        InternalContextSource::from_static("goal"),
        prompt,
    ))
}
