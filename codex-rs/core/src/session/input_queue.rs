use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::user_input::UserInput;
use serde::Serialize;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::watch;

const MAX_PENDING_MAILBOX_COMMUNICATIONS: usize = 1_024;
const MAX_SEEN_MAILBOX_COMMUNICATION_IDS: usize = 4_096;
const MAX_PENDING_TURN_INPUT_ITEMS: usize = 1_024;
const MAX_PENDING_TURN_INPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct StartInputFence {
    pub(crate) turn_id: String,
    pub(crate) cancellation: tokio_util::sync::CancellationToken,
}

impl PartialEq for StartInputFence {
    fn eq(&self, other: &Self) -> bool { self.turn_id == other.turn_id }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
        start_fence: Option<StartInputFence>,
    },
    ResponseItem(ResponseItem),
    /// A supplied source map is owned by accepted input until its ordered
    /// history transaction commits; failed startup must not consume it.
    AdditionalContext {
        values: indexmap::IndexMap<String, codex_protocol::protocol::AdditionalContextEntry>,
        start_fence: Option<StartInputFence>,
    },
    /// Model-visible runtime context generated inside the active turn. Unlike
    /// user or extension steering, this must not wake owner-held operations or
    /// force another turn solely because it is queued.
    InternalResponseItem(ResponseItem),
    InterAgentCommunication(InterAgentCommunication),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputQueueActivity {
    /// Lowest priority: a deferred internal result is queued for the next
    /// request. It is worth waking an owner-held wait, but it must never mask
    /// user steering that is also pending.
    InternalCompletion,
    Mailbox,
    Steer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingInputAdmissionError {
    pub(crate) max_items: usize,
    pub(crate) max_bytes: usize,
}

/// Turn-local pending input storage owned by the input queue flow.
#[derive(Default)]
pub(crate) struct TurnInputQueue {
    items: Vec<TurnInput>,
    bytes: usize,
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    input_receipt_ids: std::sync::Mutex<HashSet<String>>,
    // Last in the queue lock order; never held across an await. Nested tool
    // admission shares this fence with successful steering commits.
    user_intent: std::sync::Mutex<UserIntentState>,
    activity_tx: watch::Sender<InputQueueActivity>,
    agent_activity_tx: watch::Sender<InputQueueActivity>,
    startup_recovery_items: Mutex<TurnInputQueue>,
    mailbox: Mutex<MailboxState>,
    max_pending_mailbox_communications: usize,
    max_seen_mailbox_communication_ids: usize,
    max_pending_turn_input_items: usize,
    max_pending_turn_input_bytes: usize,
}

#[derive(Default)]
struct UserIntentState {
    // Epochs are only compared for equality, so steering admissions wrap the
    // counter instead of exhausting it; the epoch still changes on every one.
    accepted: u64,
    incorporated: u64,
}

#[derive(Default)]
struct MailboxState {
    pending_mails: VecDeque<(InterAgentCommunication, usize)>,
    bytes: usize,
    seen_communication_ids: HashSet<codex_protocol::ResponseItemId>,
    seen_communication_id_order: VecDeque<codex_protocol::ResponseItemId>,
}

impl InputQueue {
    pub(super) fn remember_input_receipt(&self, id: &str) -> bool {
        self.input_receipt_ids.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id.to_string())
    }

    pub(super) fn forget_input_receipt(&self, id: &str) {
        self.input_receipt_ids.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id);
    }
    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        let (agent_activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            input_receipt_ids: Default::default(),
            user_intent: std::sync::Mutex::new(UserIntentState::default()),
            activity_tx,
            agent_activity_tx,
            startup_recovery_items: Mutex::new(TurnInputQueue::default()),
            mailbox: Mutex::new(MailboxState::default()),
            max_pending_mailbox_communications: MAX_PENDING_MAILBOX_COMMUNICATIONS,
            max_seen_mailbox_communication_ids: MAX_SEEN_MAILBOX_COMMUNICATION_IDS,
            max_pending_turn_input_items: MAX_PENDING_TURN_INPUT_ITEMS,
            max_pending_turn_input_bytes: MAX_PENDING_TURN_INPUT_BYTES,
        }
    }

    pub(crate) fn incorporated_user_intent_epoch(&self) -> u64 {
        self.user_intent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .incorporated
    }

    /// Wait for successfully admitted steering newer than a request's input.
    /// Subscribe before checking the retained epoch so an arrival cannot be
    /// lost, even when a later mailbox wake overwrites the watch's value.
    pub(crate) async fn wait_for_user_intent_change(&self, authoring_epoch: u64) {
        let mut activity_rx = self.activity_tx.subscribe();
        loop {
            let accepted = self
                .user_intent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .accepted;
            if accepted != authoring_epoch {
                return;
            }
            if activity_rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// Poll the first dispatch under the steering commit fence. Once admitted,
    /// the same owner future must settle even if later steering changes intent.
    pub(crate) fn admit_code_mode_tool<T>(
        &self,
        authoring_epoch: u64,
        admit: impl FnOnce() -> T,
    ) -> Result<T, u64> {
        let intent = self
            .user_intent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if intent.accepted != authoring_epoch {
            return Err(intent.accepted);
        }
        Ok(admit())
    }



    /// Subscribes to activity wakes, then reports what is already pending.
    ///
    /// Subscribing first is what makes the pair safe: anything that lands
    /// between the two is retained state the caller sees on the second half, or
    /// a wake the receiver has already registered for.
    pub(crate) async fn subscribe_code_mode_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
        has_internal_completion: bool,
    ) -> (
        watch::Receiver<InputQueueActivity>,
        Option<InputQueueActivity>,
    ) {
        let activity_rx = self.activity_tx.subscribe();
        let pending_activity = self
            .pending_activity(turn_state, has_internal_completion)
            .await;
        (activity_rx, pending_activity)
    }

    pub(crate) async fn subscribe_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
        has_internal_completion: bool,
    ) -> (
        watch::Receiver<InputQueueActivity>,
        Option<InputQueueActivity>,
    ) {
        let (receiver, pending) = self.subscribe_agent_activity(turn_state).await;
        (
            receiver,
            pending.or(has_internal_completion.then_some(InputQueueActivity::InternalCompletion)),
        )
    }

    /// Agent waits retain their original policy: any mailbox message wakes them.
    pub(crate) async fn subscribe_agent_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
    ) -> (
        watch::Receiver<InputQueueActivity>,
        Option<InputQueueActivity>,
    ) {
        let receiver = self.agent_activity_tx.subscribe();
        let pending = self.pending_agent_activity(turn_state).await;
        (receiver, pending)
    }

    pub(crate) async fn pending_agent_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
    ) -> Option<InputQueueActivity> {
        let mut pending = self.pending_activity(turn_state, false).await;
        if pending != Some(InputQueueActivity::Steer) && self.has_pending_mailbox_items().await {
            pending = Some(InputQueueActivity::Mailbox);
        }
        pending
    }

    /// A watch is a wake hint, never a retained activity or its priority.
    pub(crate) async fn next_agent_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
        receiver: &mut watch::Receiver<InputQueueActivity>,
    ) -> InputQueueActivity {
        loop {
            receiver.borrow_and_update();
            if let Some(activity) = self.pending_agent_activity(turn_state).await {
                return activity;
            }
            if receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// Derives the highest-priority pending activity from queue state.
    ///
    /// The watch channel carries only its latest value, so a low-priority wake
    /// published after steering would otherwise hide it. Every waiter
    /// re-derives from state after each wake instead of trusting that value.
    pub(crate) async fn pending_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
        has_internal_completion: bool,
    ) -> Option<InputQueueActivity> {
        let has_recovered_steer = self
            .startup_recovery_items
            .lock()
            .await
            .iter()
            .any(TurnInput::is_steering_input);
        let has_pending_steer = if let Some(turn_state) = turn_state {
            turn_state.lock().await.pending_input.has_steering_input()
        } else {
            false
        };
        if has_recovered_steer || has_pending_steer {
            Some(InputQueueActivity::Steer)
        } else if self.has_trigger_turn_mailbox_items().await {
            Some(InputQueueActivity::Mailbox)
        } else if has_internal_completion {
            Some(InputQueueActivity::InternalCompletion)
        } else {
            None
        }
    }


    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
    ) -> Result<bool, codex_protocol::error::CodexErr> {
        let full_error = || codex_protocol::error::CodexErr::InvalidRequest(format!(
            "session mailbox is full ({} messages or {} bytes); retry after messages are consumed",
            self.max_pending_mailbox_communications, self.max_pending_turn_input_bytes
        ));
        {
            let mailbox = self.mailbox.lock().await;
            if communication.id.as_ref()
                .is_some_and(|id| mailbox.seen_communication_ids.contains(id))
            {
                return Ok(false);
            }
            if mailbox.pending_mails.len() >= self.max_pending_mailbox_communications {
                return Err(full_error());
            }
        }
        // Size outside the mutex, then repeat all admission checks atomically.
        let bytes = serialized_size(&communication);
        let mut mailbox = self.mailbox.lock().await;
        if communication
            .id
            .as_ref()
            .is_some_and(|id| mailbox.seen_communication_ids.contains(id))
        {
            return Ok(false);
        }
        if mailbox.pending_mails.len() >= self.max_pending_mailbox_communications
            || mailbox.bytes.saturating_add(bytes) > self.max_pending_turn_input_bytes
        {
            return Err(full_error());
        }
        if let Some(id) = communication.id.as_ref() {
            mailbox.seen_communication_ids.insert(id.clone());
            mailbox.seen_communication_id_order.push_back(id.clone());
            compact_seen_mailbox_ids(&mut mailbox, self.max_seen_mailbox_communication_ids);
        }
        let trigger_turn = communication.trigger_turn;
        mailbox.bytes += bytes;
        mailbox.pending_mails.push_back((communication, bytes));
        drop(mailbox);
        self.agent_activity_tx
            .send_replace(InputQueueActivity::Mailbox);
        if trigger_turn {
            self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        }
        Ok(true)
    }


    pub(crate) async fn seed_seen_mailbox_communication_ids_from_ids(
        &self,
        ids: impl IntoIterator<Item = codex_protocol::ResponseItemId>,
    ) {
        let mut mailbox = self.mailbox.lock().await;
        for id in ids {
            if mailbox.seen_communication_ids.insert(id.clone()) {
                mailbox.seen_communication_id_order.push_back(id);
                compact_seen_mailbox_ids(&mut mailbox, self.max_seen_mailbox_communication_ids);
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Read recovered input and mailbox input as one snapshot in recovery -> mailbox lock order"
    )]
    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        let recovered = self.startup_recovery_items.lock().await;
        recovered
            .iter()
            .any(|item| matches!(item, TurnInput::InterAgentCommunication(_)))
            || !self.mailbox.lock().await.pending_mails.is_empty()
    }

    /// Authors of queued mail, recovered input first, without consuming it.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Read recovered input and mailbox input as one snapshot in recovery -> mailbox lock order"
    )]
    pub(crate) async fn pending_mailbox_authors(&self) -> Vec<codex_protocol::AgentPath> {
        let recovered = self.startup_recovery_items.lock().await;
        let mut authors = recovered
            .iter()
            .filter_map(|item| match item {
                TurnInput::InterAgentCommunication(mail) => Some(mail.author.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        authors.extend(
            self.mailbox
                .lock()
                .await
                .pending_mails
                .iter()
                .map(|(mail, _)| mail.author.clone()),
        );
        authors
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Read recovered input and mailbox input as one snapshot in recovery -> mailbox lock order"
    )]
    pub(crate) async fn has_trigger_turn_mailbox_items(&self) -> bool {
        let recovered = self.startup_recovery_items.lock().await;
        recovered.iter().any(
            |item| matches!(item, TurnInput::InterAgentCommunication(mail) if mail.trigger_turn),
        ) || self
            .mailbox
            .lock()
            .await
            .pending_mails
            .iter()
            .any(|(mail, _)| mail.trigger_turn)
    }

    /// Whether recovered input should start a new turn once the current turn releases its
    /// terminal fence. User input was already accepted as turn work, while mailbox input still
    /// honors its explicit `trigger_turn` policy.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Read recovered input and mailbox input as one snapshot in recovery -> mailbox lock order"
    )]
    pub(crate) async fn has_pending_turn_start_work(&self) -> bool {
        let recovered = self.startup_recovery_items.lock().await;
        recovered.iter().any(|item| {
            matches!(
                item,
                TurnInput::UserInput { content, .. } if !content.is_empty()
            ) || matches!(
                item,
                TurnInput::InterAgentCommunication(mail) if mail.trigger_turn
            )
        }) || self
            .mailbox
            .lock()
            .await
            .pending_mails
            .iter()
            .any(|(mail, _)| mail.trigger_turn)
    }

    /// Restores already-admitted input owned by a taskless startup placeholder
    /// that was cancelled before a supervisor could take ownership. Restored
    /// items precede work accepted after the cancellation.
    pub(crate) async fn restore_transferred_startup_input(&self, input: Vec<TurnInput>) {
        if input.is_empty() {
            return;
        }
        self.restore_startup_input_groups(vec![(None, input)]).await;
    }

    pub(super) async fn restore_receipted_startup_input(&self, input: Vec<(String, Vec<TurnInput>)>) {
        self.restore_startup_input_groups(input.into_iter().map(|(id, input)| (Some(id), input)).collect()).await;
    }

    async fn restore_startup_input_groups(&self, groups: Vec<(Option<String>, Vec<TurnInput>)>) {
        let mut recovered = self.startup_recovery_items.lock().await;
        // Once recovery ownership is acquired, marker and item publication have
        // no cancellation point. A failed startup cannot strand a seen marker.
        let mut receipts = self.input_receipt_ids.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut input = Vec::new();
        let mut new_ids = Vec::new();
        for (id, items) in groups {
            if id.as_ref().is_some_and(|id| receipts.contains(id)) { continue; }
            input.extend(items);
            if let Some(id) = id { new_ids.push(id); }
        }
        let mut restored = TurnInputQueue::from_items(input);
        restored.append(&mut recovered);
        receipts.extend(new_ids);
        *recovered = restored;
        drop(receipts);
        let activity = if recovered.iter().any(TurnInput::is_steering_input) {
            Some(InputQueueActivity::Steer)
        } else if recovered.iter().any(
            |item| matches!(item, TurnInput::InterAgentCommunication(mail) if mail.trigger_turn),
        ) {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        };
        let agent_activity = activity.or_else(|| {
            recovered
                .iter()
                .any(|item| matches!(item, TurnInput::InterAgentCommunication(_)))
                .then_some(InputQueueActivity::Mailbox)
        });
        drop(recovered);
        if let Some(activity) = agent_activity {
            self.agent_activity_tx.send_replace(activity);
        }
        if let Some(activity) = activity {
            self.activity_tx.send_replace(activity);
        }
    }

    pub(crate) async fn turn_state_for_sub_id(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) -> Option<Arc<Mutex<TurnState>>> {
        let active = active_turn.lock().await;
        active.as_ref().and_then(|active_turn| {
            active_turn
                .task
                .as_ref()
                .is_some_and(|task| task.turn_context.sub_id == sub_id)
                .then(|| Arc::clone(&active_turn.turn_state))
        })
    }

    pub(crate) async fn clear_pending_for_turn_state(&self, turn_state: &Mutex<TurnState>) {
        let mut turn_state = turn_state.lock().await;
        turn_state.clear_pending_waiters();
        turn_state.input_in_flight.clear();
        turn_state.pending_input = TurnInputQueue::default();
    }

    pub(crate) async fn defer_mailbox_delivery_to_next_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        let mut turn_state = turn_state.lock().await;
        if turn_state
            .pending_input
            .items
            .iter()
            .any(TurnInput::requires_turn_continuation)
        {
            return;
        }
        turn_state.set_mailbox_delivery_phase(MailboxDeliveryPhase::NextTurn);
    }

    pub(crate) async fn accept_mailbox_delivery_for_current_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        self.accept_mailbox_delivery_for_turn_state(turn_state.as_ref())
            .await;
    }

    pub(super) async fn accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) {
        turn_state
            .lock()
            .await
            .accept_mailbox_delivery_for_current_turn();
    }

    // Both queue locks must remain held through admission so recovery and active input are
    // measured as one atomic bounded queue.
    #[allow(clippy::await_holding_invalid_type, clippy::await_holding_lock)]
    pub(super) async fn extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: &[TurnInput],
        persist: impl AsyncFnOnce() -> Result<bool, super::SteerInputError>,
        commit_context: impl FnOnce(),
    ) -> Result<(), super::SteerInputError> {
        let bytes = input
            .iter()
            .map(turn_input_size_bytes)
            .fold(0usize, usize::saturating_add);
        {
            let recovered = self.startup_recovery_items.lock().await;
            let mut turn_state = turn_state.lock().await;
            self.check_pending_turn_input_capacity(
                &recovered,
                &turn_state.pending_input,
                input.len(),
                bytes,
            ).map_err(|error| super::SteerInputError::PendingInputLimitExceeded {
                max_items: error.max_items, max_bytes: error.max_bytes,
            })?;
            // Capacity and receipt publication share the queue's existing locks.
            // No observer cancellation can split the FULL commit from insertion:
            // the session tracker owns this admission future.
            if !persist().await? { return Ok(()); }
            // All fallible admission checks are complete. The caller already
            // holds its context guards; commit them without another await before
            // publishing input or waking its consumers.
            let mut intent = self
                .user_intent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            commit_context();
            turn_state.pending_input.items.extend_from_slice(input);
            turn_state.pending_input.bytes += bytes;
            turn_state.accept_mailbox_delivery_for_current_turn();
            if input.iter().any(TurnInput::is_steering_input) {
                intent.accepted = intent.accepted.wrapping_add(1);
            }
        }
        self.activity_tx.send_replace(InputQueueActivity::Steer);
        self.agent_activity_tx
            .send_replace(InputQueueActivity::Steer);
        Ok(())
    }

    // Keep the same lock ordering and atomic capacity check as the accepting path above.
    #[allow(clippy::await_holding_invalid_type, clippy::await_holding_lock)]
    pub(crate) async fn extend_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: &[TurnInput],
    ) -> Result<(), PendingInputAdmissionError> {
        let bytes = input
            .iter()
            .map(turn_input_size_bytes)
            .fold(0usize, usize::saturating_add);
        let recovered = self.startup_recovery_items.lock().await;
        let mut turn_state = turn_state.lock().await;
        self.check_pending_turn_input_capacity(
            &recovered,
            &turn_state.pending_input,
            input.len(),
            bytes,
        )?;
        let mut intent = self
            .user_intent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        turn_state.pending_input.items.extend_from_slice(input);
        turn_state.pending_input.bytes += bytes;
        if input.iter().any(TurnInput::is_steering_input) {
            intent.accepted = intent.accepted.wrapping_add(1);
        }
        Ok(())
    }

    /// Admits model-visible input into an active turn and wakes consumers that
    /// suspend until steering activity arrives.
    pub(crate) async fn extend_pending_input_for_active_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: &[TurnInput],
    ) -> Result<(), PendingInputAdmissionError> {
        self.extend_pending_input_for_turn_state(turn_state, input)
            .await?;
        if !input.is_empty() {
            self.activity_tx.send_replace(InputQueueActivity::Steer);
            self.agent_activity_tx
                .send_replace(InputQueueActivity::Steer);
        }
        Ok(())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "queue ownership and the taskless turn reservation must remain atomic"
    )]
    pub(crate) async fn transfer_pending_input_to_turn_state(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        expected_turn_state: &Arc<Mutex<TurnState>>,
    ) -> bool {
        let active = active_turn.lock().await;
        let Some(turn) = active.as_ref().filter(|turn| {
            turn.task.is_none()
                && turn.terminal.is_none()
                && Arc::ptr_eq(&turn.turn_state, expected_turn_state)
        }) else {
            return false;
        };
        let mut recovered = self.startup_recovery_items.lock().await;
        let mut turn_state = turn.turn_state.lock().await;
        let mut mailbox = if turn_state.accepts_mailbox_delivery_for_current_turn() {
            Some(self.mailbox.lock().await)
        } else {
            None
        };

        // Keep accepted items in their owning queues until every lock is held.
        // No cancellation point may separate removal from destination insertion.
        let mut input = std::mem::take(&mut *recovered);
        input.append(&mut turn_state.pending_input);
        if let Some(mailbox) = mailbox.as_mut() {
            self.transfer_mailbox_prefix(mailbox, &mut input);
        }
        turn_state.pending_input = input;
        true
    }

    /// Called under the startup admission lock, immediately before worker
    /// installation. Earlier recovery and taskless input precede the new prompt.
    pub(crate) async fn prepare_initial_input(
        &self,
        state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) -> Vec<TurnInput> {
        let mut state = state.lock().await;
        let mut earlier = std::mem::take(&mut state.pending_input);
        earlier.items.extend(input);
        earlier.retain_live_starts();
        state.input_in_flight.extend(earlier.items.iter().cloned());
        earlier.items
    }

    fn transfer_mailbox_prefix(&self, mailbox: &mut MailboxState, input: &mut TurnInputQueue) {
        while let Some((_, bytes)) = mailbox.pending_mails.front() {
            if input.items.len() >= self.max_pending_turn_input_items
                || input.bytes.saturating_add(*bytes) > self.max_pending_turn_input_bytes
            {
                break;
            }
            let Some((mail, bytes)) = mailbox.pending_mails.pop_front() else {
                break;
            };
            mailbox.bytes -= bytes;
            input.bytes += bytes;
            input.items.push(TurnInput::InterAgentCommunication(mail));
        }
    }


    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Acquire all owners in recovery -> turn-state -> mailbox order before moving accepted mail"
    )]
    pub(crate) async fn take_initial_mailbox_for_state(
        &self,
        state: Option<&Mutex<TurnState>>,
        initial_input: &[TurnInput],
        retain_for_recording: bool,
    ) -> Vec<TurnInput> {
        let mut output = TurnInputQueue {
            items: Vec::new(),
            bytes: initial_input
                .iter()
                .map(turn_input_size_bytes)
                .fold(0usize, usize::saturating_add),
        };
        let mut recovered = self.startup_recovery_items.lock().await;
        let mut turn = match state {
            Some(state) => Some(state.lock().await),
            None => None,
        };
        if turn
            .as_ref()
            .is_some_and(|turn| !turn.accepts_mailbox_delivery_for_current_turn())
        {
            return Vec::new();
        }
        let mut mailbox = self.mailbox.lock().await;
        let take_owned_mail = |source: &mut TurnInputQueue, output: &mut TurnInputQueue| {
            let mut admitted_items = initial_input.len().saturating_add(output.items.len());
            let mut admitted_bytes = output.bytes;
            let mut fits = true;
            // Compact retained steering input once, without shifting its tail
            // for every extracted mail. Stop admission at the first full mail.
            output.items.extend(source.items.extract_if(.., |item| {
                if !fits || !matches!(item, TurnInput::InterAgentCommunication(_)) {
                    return false;
                }
                let bytes = turn_input_size_bytes(item);
                if admitted_items >= self.max_pending_turn_input_items
                    || admitted_bytes.saturating_add(bytes) > self.max_pending_turn_input_bytes
                {
                    fits = false;
                    return false;
                }
                admitted_items += 1;
                admitted_bytes += bytes;
                true
            }));
            source.bytes -= admitted_bytes - output.bytes;
            output.bytes = admitted_bytes;
            fits
        };
        if !take_owned_mail(&mut recovered, &mut output) {
            if retain_for_recording && let Some(turn) = turn.as_mut() {
                turn.input_in_flight.extend(output.items.iter().cloned());
            }
            return output.items;
        }
        if let Some(turn) = turn.as_mut()
            && !take_owned_mail(&mut turn.pending_input, &mut output)
        {
            if retain_for_recording {
                turn.input_in_flight.extend(output.items.iter().cloned());
            }
            return output.items;
        }
        while let Some((_, bytes)) = mailbox.pending_mails.front() {
            if initial_input.len().saturating_add(output.items.len())
                >= self.max_pending_turn_input_items
                || output.bytes.saturating_add(*bytes) > self.max_pending_turn_input_bytes
            {
                break;
            }
            let Some((mail, bytes)) = mailbox.pending_mails.pop_front() else {
                break;
            };
            mailbox.bytes -= bytes;
            output.bytes += bytes;
            output.items.push(TurnInput::InterAgentCommunication(mail));
        }
        if retain_for_recording && let Some(turn) = turn.as_mut() {
            turn.input_in_flight.extend(output.items.iter().cloned());
        }
        output.items
    }

    fn check_pending_turn_input_capacity(
        &self,
        recovered: &TurnInputQueue,
        active: &TurnInputQueue,
        incoming_items: usize,
        incoming_bytes: usize,
    ) -> Result<(), PendingInputAdmissionError> {
        let item_count = recovered
            .items
            .len()
            .saturating_add(active.items.len())
            .saturating_add(incoming_items);
        let byte_count = recovered
            .bytes
            .saturating_add(active.bytes)
            .saturating_add(incoming_bytes);
        if item_count > self.max_pending_turn_input_items
            || byte_count > self.max_pending_turn_input_bytes
        {
            return Err(PendingInputAdmissionError {
                max_items: self.max_pending_turn_input_items,
                max_bytes: self.max_pending_turn_input_bytes,
            });
        }
        Ok(())
    }


    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Both queues must be locked before draining to preserve input if acquisition is cancelled"
    )]
    pub(crate) async fn recover_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> bool {
        let mut recovered = self.startup_recovery_items.lock().await;
        let mut turn_state = turn_state.lock().await;
        let has_input =
            !turn_state.input_in_flight.is_empty() || !turn_state.pending_input.items.is_empty();
        let mut in_flight =
            TurnInputQueue::from_items(turn_state.input_in_flight.drain(..).collect());
        recovered.append(&mut in_flight);
        recovered.append(&mut turn_state.pending_input);
        has_input
    }


    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Every queue owner must be held before draining so cancelled acquisition preserves accepted input"
    )]
    pub(crate) async fn take_pending_for_state(
        &self,
        state: Option<&Mutex<TurnState>>,
        retain_for_recording: bool,
    ) -> Vec<TurnInput> {
        // Acquire every required queue before removing accepted input. Cancellation while
        // waiting for any lock must leave all input available to the next consumer.
        // Match active-turn injection's active -> recovery -> turn-state lock order.
        let mut recovered = self.startup_recovery_items.lock().await;
        let mut turn_state = match state {
            Some(state) => Some(state.lock().await),
            None => None,
        };
        let accepts_mailbox_delivery = turn_state
            .as_ref()
            .is_none_or(|state| state.accepts_mailbox_delivery_for_current_turn());
        let mut mailbox = if accepts_mailbox_delivery {
            Some(self.mailbox.lock().await)
        } else {
            None
        };

        let mut input = std::mem::take(&mut *recovered);
        if let Some(turn_state) = turn_state.as_mut() {
            input.append(&mut turn_state.pending_input);
        }
        if let Some(mailbox) = mailbox.as_mut() {
            self.transfer_mailbox_prefix(mailbox, &mut input);
        }
        input.retain_live_starts();
        if retain_for_recording && let Some(turn_state) = turn_state.as_mut() {
            turn_state
                .input_in_flight
                .extend(input.items.iter().cloned());
        }
        // All steering in these queues is now owned by the history recording
        // transaction. Later arrivals cannot acquire this request's authority.
        if retain_for_recording {
            let mut intent = self
                .user_intent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            intent.incorporated = intent.accepted;
        }
        input.items
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state reads must remain atomic"
    )]
    pub(crate) async fn has_pending_input(&self, active_turn: &Mutex<Option<ActiveTurn>>) -> bool {
        if self
            .startup_recovery_items
            .lock()
            .await
            .iter()
            .any(TurnInput::requires_turn_continuation)
        {
            return true;
        }
        let (has_turn_pending_input, accepts_mailbox_delivery) = {
            let active = active_turn.lock().await;
            match active.as_ref() {
                Some(active_turn) => {
                    let turn_state = active_turn.turn_state.lock().await;
                    (
                        turn_state
                            .pending_input
                            .items
                            .iter()
                            .any(TurnInput::requires_turn_continuation),
                        turn_state.accepts_mailbox_delivery_for_current_turn(),
                    )
                }
                None => (false, true),
            }
        };
        if has_turn_pending_input {
            return true;
        }
        if !accepts_mailbox_delivery {
            return false;
        }
        self.has_trigger_turn_mailbox_items().await
    }
}

fn compact_seen_mailbox_ids(mailbox: &mut MailboxState, max_seen_ids: usize) {
    while mailbox.seen_communication_id_order.len() > max_seen_ids {
        let Some(expired_id) = mailbox.seen_communication_id_order.pop_front() else {
            break;
        };
        mailbox.seen_communication_ids.remove(&expired_id);
    }
}


fn turn_input_size_bytes(input: &TurnInput) -> usize {
    match input {
        TurnInput::UserInput { content, client_id, .. } => serialized_size(&(content, client_id)),
        TurnInput::ResponseItem(item) | TurnInput::InternalResponseItem(item) => {
            serialized_size(item)
        }
        TurnInput::InterAgentCommunication(communication) => serialized_size(communication),
        TurnInput::AdditionalContext { values, .. } => serialized_size(values),
    }
}

fn serialized_size(value: &impl Serialize) -> usize {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value).map_or(usize::MAX, |()| counter.bytes)
}

#[derive(Default)]
struct ByteCounter {
    bytes: usize,
}

impl io::Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl TurnInputQueue {
    fn from_items(mut items: Vec<TurnInput>) -> Self {
        items.retain(|item| !item.is_cancelled_start());
        let bytes = items
            .iter()
            .map(turn_input_size_bytes)
            .fold(0usize, usize::saturating_add);
        Self { items, bytes }
    }

    fn append(&mut self, other: &mut Self) {
        self.bytes = self.bytes.saturating_add(std::mem::take(&mut other.bytes));
        self.items.append(&mut other.items);
    }

    fn retain_live_starts(&mut self) {
        self.items.retain(|item| !item.is_cancelled_start());
        self.bytes = self.items.iter().map(turn_input_size_bytes).fold(0, usize::saturating_add);
    }

    fn iter(&self) -> impl Iterator<Item = &TurnInput> {
        self.items.iter().filter(|item| !item.is_cancelled_start())
    }



    fn has_steering_input(&self) -> bool {
        self.iter().any(TurnInput::is_steering_input)
    }
}

impl TurnInput {
    pub(crate) fn is_cancelled_start(&self) -> bool {
        let fence = match self {
            Self::UserInput { start_fence, .. } | Self::AdditionalContext { start_fence, .. } => start_fence,
            _ => return false,
        };
        fence.as_ref().is_some_and(|fence| fence.cancellation.is_cancelled())
    }
    fn is_steering_input(&self) -> bool {
        if self.is_cancelled_start() { return false; }
        matches!(
            self,
            Self::UserInput { .. } | Self::ResponseItem(_) | Self::AdditionalContext { .. }
        )
    }

    fn requires_turn_continuation(&self) -> bool {
        if self.is_cancelled_start() { return false; }
        match self {
            Self::InternalResponseItem(_) => false,
            Self::InterAgentCommunication(mail) => mail.trigger_turn,
            Self::UserInput { .. } | Self::ResponseItem(_) | Self::AdditionalContext { .. } => true,
        }
    }
}

