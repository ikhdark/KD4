use super::input_queue::TurnInput;
use super::session::Session;
use super::turn_context::TurnContext;
use crate::codex_thread::InjectResponseItemsError;
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
    ) -> Result<(), InjectResponseItemsError> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            // Terminalization has detached the worker and may already have drained its queue.
            // Let the caller record late context in history instead of losing it on detach.
            Some(active_turn) if active_turn.task.is_none() && active_turn.terminal.is_some() => {
                Err(InjectResponseItemsError::NoActiveTurn(input))
            }
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
                    .map_err(
                        |error| InjectResponseItemsError::PendingInputLimitExceeded {
                            input,
                            max_items: error.max_items,
                            max_bytes: error.max_bytes,
                        },
                    )?;
                Ok(())
            }
            None => Err(InjectResponseItemsError::NoActiveTurn(input)),
        }
    }

    /// Injects model-visible context produced by the active runtime without
    /// classifying it as user steering or waking owner-held tool execution.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    async fn inject_internal_if_running(
        &self,
        input: Vec<ResponseItem>,
    ) -> Result<(), InjectResponseItemsError> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            // Terminalization has detached the worker and may already have drained its queue.
            // Let the caller record late context in history instead of losing it on detach.
            Some(active_turn) if active_turn.task.is_none() && active_turn.terminal.is_some() => {
                Err(InjectResponseItemsError::NoActiveTurn(input))
            }
            Some(active_turn) => {
                let pending_input = input
                    .iter()
                    .cloned()
                    .map(TurnInput::InternalResponseItem)
                    .collect::<Vec<_>>();
                self.input_queue
                    .extend_pending_input_for_turn_state(
                        active_turn.turn_state.as_ref(),
                        &pending_input,
                    )
                    .await
                    .map_err(
                        |error| InjectResponseItemsError::PendingInputLimitExceeded {
                            input,
                            max_items: error.max_items,
                            max_bytes: error.max_bytes,
                        },
                    )?;
                Ok(())
            }
            None => Err(InjectResponseItemsError::NoActiveTurn(input)),
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
        if self.input_queue.has_pending_turn_start_work().await {
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTurnStartWork,
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
        if self.input_queue.has_pending_turn_start_work().await {
            drop(task_start_permit);
            self.maybe_start_turn_for_pending_work().await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTurnStartWork,
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

        if self.input_queue.has_pending_turn_start_work().await {
            self.clear_reserved_idle_turn(&turn_state).await;
            drop(task_start_permit);
            self.maybe_start_turn_for_pending_work().await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTurnStartWork,
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
        if self.input_queue.has_pending_turn_start_work().await {
            self.clear_reserved_idle_turn(&turn_state).await;
            drop(task_start_permit);
            self.maybe_start_turn_for_pending_work().await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTurnStartWork,
                input,
            ));
        }
        let pending_input = input
            .iter()
            .cloned()
            .map(TurnInput::ResponseItem)
            .collect::<Vec<_>>();
        if let Err(reason) = self
            .admit_reserved_idle_input(&turn_state, &pending_input)
            .await
        {
            self.clear_reserved_idle_turn(&turn_state).await;
            return Err(TryStartTurnIfIdleError::new(reason, input));
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

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    async fn admit_reserved_idle_input(
        &self,
        turn_state: &Arc<tokio::sync::Mutex<TurnState>>,
        input: &[TurnInput],
    ) -> Result<(), TryStartTurnIfIdleRejectionReason> {
        // Interrupt recovery takes this same lock before draining a taskless
        // reservation. Keep identity validation and input admission atomic so
        // accepted input cannot be inserted into an already-detached state.
        let active = self.active_turn.lock().await;
        if !active.as_ref().is_some_and(|turn| {
            turn.task.is_none()
                && turn.terminal.is_none()
                && Arc::ptr_eq(&turn.turn_state, turn_state)
        }) {
            return Err(TryStartTurnIfIdleRejectionReason::Busy);
        }
        self.input_queue
            .extend_pending_input_for_turn_state(turn_state.as_ref(), input)
            .await
            .map_err(|_| TryStartTurnIfIdleRejectionReason::PendingInputLimitExceeded)
    }

    async fn clear_reserved_idle_turn(
        self: &Arc<Self>,
        turn_state: &Arc<tokio::sync::Mutex<TurnState>>,
    ) {
        self.clear_taskless_placeholder(turn_state).await;
    }

    /// Injects items into active work, or records them without starting a turn.
    pub(crate) async fn inject_no_new_turn(
        &self,
        items: Vec<ResponseItem>,
        current_turn_context: Option<&TurnContext>,
    ) -> codex_protocol::error::Result<()> {
        let items = match self.inject_if_running(items).await {
            Ok(()) => return Ok(()),
            Err(InjectResponseItemsError::NoActiveTurn(items)) => items,
            Err(error) => {
                return Err(codex_protocol::error::CodexErr::InvalidRequest(
                    error.to_string(),
                ));
            }
        };
        let default_turn_context;
        let turn_context = match current_turn_context {
            Some(turn_context) => turn_context,
            None => {
                default_turn_context = self.new_default_turn().await;
                default_turn_context.as_ref()
            }
        };
        self.record_conversation_items(turn_context, &items).await?;
        Ok(())
    }

    /// Injects internal runtime context into active work, or records it without
    /// starting a turn. Internal context is drained normally for model
    /// projection, but it is never treated as fresh user steering.
    pub(crate) async fn inject_internal_no_new_turn(
        self: &Arc<Self>,
        items: Vec<ResponseItem>,
        current_turn_context: Option<&Arc<TurnContext>>,
    ) -> std::io::Result<()> {
        let items = match self.inject_internal_if_running(items).await {
            Ok(()) => return Ok(()),
            Err(InjectResponseItemsError::NoActiveTurn(items)) => items,
            Err(error) => return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, error)),
        };
        let default_turn_context;
        let turn_context = match current_turn_context {
            Some(turn_context) => turn_context,
            None => {
                default_turn_context = self.new_default_turn().await;
                &default_turn_context
            }
        };
        // Once fallback recording is accepted, own both physical and live history
        // across notification cancellation. Active-turn queue admission above
        // remains cancellation-safe before it inserts any item.
        // Each notification is a distinct emission, including repeated text from
        // the same cell. Assign its normal response IDs before the commit retry key
        // is formed so intentional repeated notifications are not deduplicated.
        let items = self.prepare_conversation_items_for_history(turn_context, &items);
        self.record_conversation_items_ordered(turn_context, &items)
            .await
    }
}

