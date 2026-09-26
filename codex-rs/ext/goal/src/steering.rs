use codex_core::context::ContextualUserFragment;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
use codex_prompts::budget_limit_prompt;
use codex_prompts::continuation_prompt;
use codex_prompts::objective_updated_prompt;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadGoal;

pub(crate) fn budget_limit_steering_item(goal: &ThreadGoal, goal_id: &str) -> ResponseItem {
    goal_context_input_item(
        budget_limit_prompt(goal),
        crate::tool::goal_reference_for(goal_id, &goal.objective),
    )
}

pub(crate) fn objective_updated_steering_item(goal: &codex_state::ThreadGoal) -> ResponseItem {
    goal_context_input_item(
        objective_updated_prompt(&crate::tool::protocol_goal_from_state(goal.clone())),
        crate::tool::goal_reference(goal),
    )
}

pub(crate) fn continuation_steering_item(goal: &codex_state::ThreadGoal) -> ResponseItem {
    goal_context_input_item(
        continuation_prompt(&crate::tool::protocol_goal_from_state(goal.clone())),
        crate::tool::goal_reference(goal),
    )
}

/// Every goal notice names the reference `update_goal` requires, so the model can act on it.
fn goal_context_input_item(prompt: String, reference: String) -> ResponseItem {
    ContextualUserFragment::into(InternalModelContextFragment::new(
        InternalContextSource::from_static("goal"),
        format!("{prompt}\nGoal reference for update_goal: {reference}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::ThreadId;
    use codex_protocol::models::ContentItem;
    use sha2::Digest;

    fn item_text(item: ResponseItem) -> String {
        let ResponseItem::Message { content, .. } = item else {
            panic!("goal steering must be a message: {item:?}");
        };
        content
            .into_iter()
            .map(|content| match content {
                ContentItem::InputText { text } => text,
                other => panic!("goal steering must be text: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn every_goal_steering_item_names_the_update_goal_reference() {
        let now = chrono::Utc::now();
        let goal = codex_state::ThreadGoal {
            thread_id: ThreadId::new(),
            goal_id: "goal-1".to_string(),
            objective: "finish the stack".to_string(),
            status: codex_state::ThreadGoalStatus::BudgetLimited,
            token_budget: Some(10),
            tokens_used: 12,
            time_used_seconds: 3,
            created_at: now,
            updated_at: now,
        };
        let expected = format!(
            "goal-1:{:x}",
            sha2::Sha256::digest("finish the stack".as_bytes())
        );
        // The reference must be the one update_goal validates against.
        assert_eq!(crate::tool::goal_reference(&goal), expected);
        let line = format!("\nGoal reference for update_goal: {expected}");

        let budget_limit = item_text(budget_limit_steering_item(
            &crate::tool::protocol_goal_from_state(goal.clone()),
            &goal.goal_id,
        ));
        assert!(budget_limit.contains("Tokens used: 12"), "{budget_limit}");
        for text in [
            budget_limit,
            item_text(continuation_steering_item(&goal)),
            item_text(objective_updated_steering_item(&goal)),
        ] {
            assert!(text.contains(&line), "{text}");
        }
    }
}
