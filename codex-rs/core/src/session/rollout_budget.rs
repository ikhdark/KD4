use super::session::Session;
use super::turn_context::TurnContext;
use crate::context::ContextualUserFragment;
use crate::rollout_budget::ROLLOUT_BUDGET_APPROVAL_PHRASE;
use crate::session::input_queue::TurnInput;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::user_input::UserInput;

pub(super) fn maybe_approve_additional_tranche(sess: &Session, input: &[TurnInput]) {
    let [TurnInput::UserInput { content, .. }] = input else {
        return;
    };
    let [UserInput::Text { text, .. }] = content.as_slice() else {
        return;
    };
    if text
        .trim()
        .eq_ignore_ascii_case(ROLLOUT_BUDGET_APPROVAL_PHRASE)
    {
        sess.services
            .agent_control
            .rollout_budget()
            .approve_additional_tranche();
    }
}

pub(super) async fn maybe_record_reminder(
    sess: &Session,
    turn_context: &TurnContext,
    window_id: &str,
) {
    let budget = sess.services.agent_control.rollout_budget();
    let Some(reminder) = budget.pending_reminder(sess.thread_id(), window_id) else {
        return;
    };
    let response_item = ContextualUserFragment::into(crate::context::RolloutBudgetContext {
        remaining_tokens: reminder.remaining_tokens,
    });
    sess.record_conversation_items(turn_context, std::slice::from_ref(&response_item))
        .await;
    budget.mark_reminder_delivered(sess.thread_id(), window_id, reminder);
}

impl Session {
    pub(crate) fn reserve_rollout_model_call(&self, turn_context: &TurnContext) -> CodexResult<()> {
        if !self
            .services
            .agent_control
            .rollout_budget()
            .try_reserve_model_call(&turn_context.sub_id)
        {
            return Err(CodexErr::SessionBudgetExceeded);
        }
        Ok(())
    }

    pub(crate) fn record_rollout_budget_usage(
        &self,
        turn_context: &TurnContext,
        usage: &TokenUsage,
    ) -> CodexResult<()> {
        if self
            .services
            .agent_control
            .rollout_budget()
            .record_usage(usage, &turn_context.sub_id)
        {
            return Err(CodexErr::SessionBudgetExceeded);
        }
        Ok(())
    }
}
