use super::input_queue::TurnInput;
use super::session::Session;
use super::turn_context::TurnContext;
use crate::codex_thread::TryStartTurnIfIdleError;
use crate::codex_thread::TryStartTurnIfIdleRejectionReason;
use crate::state::ActiveTurn;
use crate::state::TurnState;
use crate::tasks::RegularTask;
use crate::tasks::TasklessTurnStartupGuard;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::ResponseItem;
use std::sync::Arc;

impl Session {
    /// Returns the input if there is no active turn to inject into.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn inject_if_running(
        &self,
        input: Vec<ResponseItem>,
    ) -> Result<(), Vec<ResponseItem>> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            Some(active_turn) => {
                let pending_input = input
                    .iter()
                    .cloned()
                    .map(TurnInput::ResponseItem)
                    .collect::<Vec<_>>();
                self.input_queue
                    .extend_pending_input_for_active_turn_state(
                        active_turn.turn_state.as_ref(),
                        &pending_input,
                    )
                    .await
                    .map_err(|_| input)?;
                Ok(())
            }
            None => Err(input),
        }
    }

    /// Starts a regular turn with the provided items only if automatic idle work
    /// is allowed for the current session state.
    ///
    /// This is the shared gate for extension-initiated idle work. It refuses to
    /// start a turn when user/client-triggered work is queued, any task is still
    /// active, or the session is currently in Plan mode. Active Review tasks are
    /// covered by the active-task check because Review turns are not steerable.
    pub(crate) async fn try_start_turn_if_idle(
        self: &Arc<Self>,
        input: Vec<ResponseItem>,
    ) -> Result<(), TryStartTurnIfIdleError> {
        if input.is_empty() {
            return Ok(());
        }
        if self.input_queue.has_trigger_turn_mailbox_items().await {
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTriggerTurn,
                input,
            ));
        }
        if self.collaboration_mode().await.mode == ModeKind::Plan {
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PlanMode,
                input,
            ));
        }

        let Ok(task_start_permit) = self.task_start_gate.acquire().await else {
            unreachable!("session-owned task-start semaphore is never closed");
        };
        if self.input_queue.has_trigger_turn_mailbox_items().await {
            drop(task_start_permit);
            self.maybe_start_turn_for_pending_work().await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTriggerTurn,
                input,
            ));
        }
        if self.collaboration_mode().await.mode == ModeKind::Plan {
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PlanMode,
                input,
            ));
        }

        let turn_state = {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.is_some() {
                return Err(TryStartTurnIfIdleError::new(
                    TryStartTurnIfIdleRejectionReason::Busy,
                    input,
                ));
            }
            let active_turn = active_turn.get_or_insert_with(ActiveTurn::default);
            Arc::clone(&active_turn.turn_state)
        };
        let mut startup_guard = TasklessTurnStartupGuard::new(self, Arc::clone(&turn_state));

        if self.input_queue.has_trigger_turn_mailbox_items().await {
            self.clear_reserved_idle_turn(&turn_state).await;
            drop(task_start_permit);
            self.maybe_start_turn_for_pending_work().await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTriggerTurn,
                input,
            ));
        }

        let turn_context = self
            .new_default_turn_with_sub_id(uuid::Uuid::new_v4().to_string())
            .await;
        if turn_context.collaboration_mode.mode == ModeKind::Plan {
            self.clear_reserved_idle_turn(&turn_state).await;
            drop(task_start_permit);
            self.maybe_start_turn_for_pending_work().await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PlanMode,
                input,
            ));
        }
        self.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
            .await;
        if self.input_queue.has_trigger_turn_mailbox_items().await {
            self.clear_reserved_idle_turn(&turn_state).await;
            drop(task_start_permit);
            self.maybe_start_turn_for_pending_work().await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTriggerTurn,
                input,
            ));
        }
        let still_reserved = {
            let active_turn = self.active_turn.lock().await;
            active_turn.as_ref().is_some_and(|active_turn| {
                active_turn.task.is_none() && Arc::ptr_eq(&active_turn.turn_state, &turn_state)
            })
        };
        if !still_reserved {
            self.clear_reserved_idle_turn(&turn_state).await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::Busy,
                input,
            ));
        }

        let pending_input = input
            .iter()
            .cloned()
            .map(TurnInput::ResponseItem)
            .collect::<Vec<_>>();
        if self
            .input_queue
            .extend_pending_input_for_turn_state(turn_state.as_ref(), &pending_input)
            .await
            .is_err()
        {
            self.clear_reserved_idle_turn(&turn_state).await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingInputLimitExceeded,
                input,
            ));
        }
        let start_result = self
            .start_task_with_admission(
                &task_start_permit,
                turn_context,
                Vec::new(),
                RegularTask::new(),
            )
            .await;
        if start_result.is_err() {
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::Busy,
                input,
            ));
        }
        startup_guard.disarm();
        Ok(())
    }

    async fn clear_reserved_idle_turn(&self, turn_state: &Arc<tokio::sync::Mutex<TurnState>>) {
        self.clear_taskless_placeholder(turn_state).await;
    }

    /// Injects items into active work, or records them without starting a turn.
    pub(crate) async fn inject_no_new_turn(
        &self,
        items: Vec<ResponseItem>,
        current_turn_context: Option<&TurnContext>,
    ) {
        let Err(items) = self.inject_if_running(items).await else {
            return;
        };
        let default_turn_context;
        let turn_context = match current_turn_context {
            Some(turn_context) => turn_context,
            None => {
                default_turn_context = self.new_default_turn().await;
                default_turn_context.as_ref()
            }
        };
        self.record_conversation_items(turn_context, &items).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::InputQueueActivity;
    use codex_protocol::models::ContentItem;

    fn objective_update_item() -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "The active goal objective was updated.".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    #[tokio::test]
    async fn injected_response_item_is_pending_steering_before_subscription() {
        let (session, _turn_context) = crate::session::tests::make_session_and_context().await;
        let turn_state = {
            let mut active_turn = session.active_turn.lock().await;
            Arc::clone(
                &active_turn
                    .get_or_insert_with(ActiveTurn::default)
                    .turn_state,
            )
        };

        session
            .inject_if_running(vec![objective_update_item()])
            .await
            .expect("active-turn injection should succeed");

        let (_activity_rx, pending_activity) = session
            .input_queue
            .subscribe_activity(Some(turn_state.as_ref()))
            .await;
        assert_eq!(pending_activity, Some(InputQueueActivity::Steer));
    }
}
