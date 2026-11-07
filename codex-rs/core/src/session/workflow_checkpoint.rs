use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;

use super::session::Session;
use super::turn_context::TurnContext;
use crate::context::ContextualUserFragment;
use crate::context_manager::is_user_turn_boundary;

#[derive(Default)]
pub(crate) struct WorkflowCheckpointState {
    tool_outputs_since_user: u64,
    last_workflow_checkpoint: u64,
}

impl WorkflowCheckpointState {
    pub(super) fn note_recorded_items(&mut self, items: &[ResponseItem]) {
        for item in items {
            let user_boundary = is_user_turn_boundary(item);
            let tool_output = matches!(
                item,
                ResponseItem::FunctionCallOutput { .. }
                    | ResponseItem::CustomToolCallOutput { .. }
                    | ResponseItem::ToolSearchOutput { .. }
            );
            if user_boundary {
                self.tool_outputs_since_user = 0;
                self.last_workflow_checkpoint = 0;
            }
            if tool_output {
                self.tool_outputs_since_user = self.tool_outputs_since_user.saturating_add(1);
            }
        }
    }

    fn take_workflow_checkpoint_due(&mut self) -> bool {
        // A sparse advisory, never a generation budget or an assertion that
        // inspection made no progress. Doubling bounds repeated prompt cost.
        let due = self.tool_outputs_since_user >= 16
            && (self.last_workflow_checkpoint == 0
                || self.tool_outputs_since_user / 2 >= self.last_workflow_checkpoint);
        if due {
            self.last_workflow_checkpoint = self.tool_outputs_since_user;
        }
        due
    }
}

pub(super) async fn maybe_record_workflow_checkpoint(
    sess: &Session,
    turn_context: &TurnContext,
) -> CodexResult<()> {
    let workflow_checkpoint = {
        let mut state = sess.state.lock().await;
        state
            .workflow_checkpoint
            .take_workflow_checkpoint_due()
            .then(|| crate::context::WorkflowCheckpoint::for_history(state.history.raw_items()))
    };
    if let Some(checkpoint) = workflow_checkpoint {
        let item = ContextualUserFragment::into(checkpoint);
        sess.record_conversation_items(turn_context, std::slice::from_ref(&item))
            .await?;
    }
    Ok(())
}
