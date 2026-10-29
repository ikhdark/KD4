use chrono::DateTime;
use chrono::Utc;
use codex_features::CurrentTimeReminderDeliveryMode;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;

use super::session::Session;
use super::turn_context::TurnContext;
use crate::context::ContextualUserFragment;
use crate::context_manager::is_user_turn_boundary;

#[derive(Default)]
pub(crate) struct CurrentTimeReminderState {
    last_delivery_time: Option<DateTime<Utc>>,
    last_window_id: Option<String>,
    pending_user_or_tool_output_boundary: bool,
    tool_outputs_since_user: u64,
    last_workflow_checkpoint: u64,
}

impl CurrentTimeReminderState {
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
            self.pending_user_or_tool_output_boundary |= user_boundary || tool_output;
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

    fn take_reminder_due(
        &mut self,
        window_id: &str,
        current_time: DateTime<Utc>,
        interval_seconds: u64,
        delivery_mode: CurrentTimeReminderDeliveryMode,
    ) -> bool {
        let is_new_window = self.last_window_id.as_deref() != Some(window_id);
        // Consume the boundary for this inference even if the interval suppresses delivery.
        let follows_user_or_tool_output =
            std::mem::take(&mut self.pending_user_or_tool_output_boundary);
        if delivery_mode == CurrentTimeReminderDeliveryMode::AfterUserOrToolOutput
            && !is_new_window
            && !follows_user_or_tool_output
        {
            return false;
        }

        let reminder_is_due = is_new_window
            || interval_seconds == 0
            || self.last_delivery_time.is_none_or(|last_delivery_time| {
                current_time
                    .signed_duration_since(last_delivery_time)
                    .num_seconds()
                    >= i64::try_from(interval_seconds).unwrap_or(i64::MAX)
            });

        if reminder_is_due {
            self.last_delivery_time = Some(current_time);
            self.last_window_id = Some(window_id.to_string());
        }

        reminder_is_due
    }
}

pub(super) async fn maybe_record_current_time_reminder(
    sess: &Session,
    turn_context: &TurnContext,
    window_id: &str,
) -> CodexResult<()> {
    // Reuse the existing pre-sampling hook. Workflow checkpoints do not enable
    // clock reminders, read the clock, or alter the current-time feature gate.
    let workflow_checkpoint_due = {
        let mut state = sess.state.lock().await;
        state.current_time_reminder.take_workflow_checkpoint_due()
    };
    if workflow_checkpoint_due {
        let item = ContextualUserFragment::into(crate::context::WorkflowCheckpoint);
        sess.record_conversation_items(turn_context, std::slice::from_ref(&item))
            .await?;
    }

    let Some(config) = turn_context.config.current_time_reminder else {
        return Ok(());
    };

    let current_time = sess
        .services
        .time_provider
        .current_time(sess.thread_id)
        .await
        .map_err(|err| CodexErr::Fatal(format!("failed to read current time: {err:#}")))?;

    let reminder_is_due = {
        let mut state = sess.state.lock().await;
        state.current_time_reminder.take_reminder_due(
            window_id,
            current_time,
            config.reminder_interval_seconds,
            config.delivery_mode,
        )
    };
    if !reminder_is_due {
        return Ok(());
    }

    let response_item =
        ContextualUserFragment::into(crate::context::CurrentTimeReminder::new(current_time));
    sess.record_conversation_items(turn_context, std::slice::from_ref(&response_item))
        .await?;

    Ok(())
}
