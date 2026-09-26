use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
#[cfg(test)]
use codex_protocol::protocol::RolloutItem;
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

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
    },
    ResponseItem(ResponseItem),
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
    activity_tx: watch::Sender<InputQueueActivity>,
    startup_recovery_items: Mutex<TurnInputQueue>,
    mailbox: Mutex<MailboxState>,
    max_pending_mailbox_communications: usize,
    max_seen_mailbox_communication_ids: usize,
    max_pending_turn_input_items: usize,
    max_pending_turn_input_bytes: usize,
}

#[derive(Default)]
struct MailboxState {
    pending_mails: VecDeque<(InterAgentCommunication, usize)>,
    bytes: usize,
    seen_communication_ids: HashSet<codex_protocol::ResponseItemId>,
    seen_communication_id_order: VecDeque<codex_protocol::ResponseItemId>,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            startup_recovery_items: Mutex::new(TurnInputQueue::default()),
            mailbox: Mutex::new(MailboxState::default()),
            max_pending_mailbox_communications: MAX_PENDING_MAILBOX_COMMUNICATIONS,
            max_seen_mailbox_communication_ids: MAX_SEEN_MAILBOX_COMMUNICATION_IDS,
            max_pending_turn_input_items: MAX_PENDING_TURN_INPUT_ITEMS,
            max_pending_turn_input_bytes: MAX_PENDING_TURN_INPUT_BYTES,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_mailbox_limits(max_pending: usize, max_seen_ids: usize) -> Self {
        let mut queue = Self::new();
        queue.max_pending_mailbox_communications = max_pending;
        queue.max_seen_mailbox_communication_ids = max_seen_ids;
        queue
    }

    #[cfg(test)]
    fn with_pending_turn_input_limits(max_items: usize, max_bytes: usize) -> Self {
        let mut queue = Self::new();
        queue.max_pending_turn_input_items = max_items;
        queue.max_pending_turn_input_bytes = max_bytes;
        queue
    }

    /// Subscribes to activity wakes, then reports what is already pending.
    ///
    /// Subscribing first is what makes the pair safe: anything that lands
    /// between the two is retained state the caller sees on the second half, or
    /// a wake the receiver has already registered for.
    pub(crate) async fn subscribe_activity(
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
        } else if self.has_pending_mailbox_items().await {
            Some(InputQueueActivity::Mailbox)
        } else if has_internal_completion {
            Some(InputQueueActivity::InternalCompletion)
        } else {
            None
        }
    }

    /// Wakes owner-held waits after an internal completion was recorded.
    ///
    /// This is only a nudge: the record, not this signal, is the retained state
    /// a waiter checks, and the waiter re-derives priority for itself.
    #[cfg(test)]
    pub(crate) fn publish_internal_completion(&self) {
        self.activity_tx
            .send_replace(InputQueueActivity::InternalCompletion);
    }

    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
    ) -> Result<bool, codex_protocol::error::CodexErr> {
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
            return Err(codex_protocol::error::CodexErr::InvalidRequest(format!(
                "session mailbox is full ({} messages or {} bytes); retry after messages are consumed",
                self.max_pending_mailbox_communications, self.max_pending_turn_input_bytes
            )));
        }
        if let Some(id) = communication.id.as_ref() {
            mailbox.seen_communication_ids.insert(id.clone());
            mailbox.seen_communication_id_order.push_back(id.clone());
            compact_seen_mailbox_ids(&mut mailbox, self.max_seen_mailbox_communication_ids);
        }
        mailbox.bytes += bytes;
        mailbox.pending_mails.push_back((communication, bytes));
        drop(mailbox);
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) async fn seed_seen_mailbox_communication_ids(&self, items: &[RolloutItem]) {
        let ids = items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::InterAgentCommunication(communication) => communication.id.as_ref(),
                RolloutItem::ResponseItem(ResponseItem::AgentMessage { id, .. }) => id.as_ref(),
                _ => None,
            })
            .cloned()
            .collect::<Vec<_>>();
        self.seed_seen_mailbox_communication_ids_from_ids(ids).await;
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

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Read recovered input and mailbox input as one snapshot in recovery -> mailbox lock order"
    )]
    #[cfg(test)]
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
        let mut restored = TurnInputQueue::from_items(input);
        let mut recovered = self.startup_recovery_items.lock().await;
        restored.append(&mut recovered);
        *recovered = restored;
        let activity = if recovered.iter().any(TurnInput::is_steering_input) {
            Some(InputQueueActivity::Steer)
        } else if recovered
            .iter()
            .any(|item| matches!(item, TurnInput::InterAgentCommunication(_)))
        {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        };
        drop(recovered);
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
        commit_context: impl FnOnce(),
    ) -> Result<(), PendingInputAdmissionError> {
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
            )?;
            // All fallible admission checks are complete. The caller already
            // holds its context guards; commit them without another await before
            // publishing input or waking its consumers.
            commit_context();
            turn_state.pending_input.items.extend_from_slice(input);
            turn_state.pending_input.bytes += bytes;
            turn_state.accept_mailbox_delivery_for_current_turn();
        }
        self.activity_tx.send_replace(InputQueueActivity::Steer);
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
        turn_state.pending_input.items.extend_from_slice(input);
        turn_state.pending_input.bytes += bytes;
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

    /// Takes already-ready mail for an initial user request, leaving later
    /// steering and recovered user work in their owning queues.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Acquire all owners before moving accepted mail"
    )]
    pub(crate) async fn take_mailbox_for_initial_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        initial_input: &[TurnInput],
    ) -> Vec<TurnInput> {
        let mut output = TurnInputQueue {
            items: Vec::new(),
            bytes: initial_input
                .iter()
                .map(turn_input_size_bytes)
                .fold(0usize, usize::saturating_add),
        };
        let active = active_turn.lock().await;
        let mut recovered = self.startup_recovery_items.lock().await;
        let mut turn = match active.as_ref() {
            Some(turn) => Some(turn.turn_state.lock().await),
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
            let mut index = 0;
            while index < source.items.len() {
                if !matches!(source.items[index], TurnInput::InterAgentCommunication(_)) {
                    index += 1;
                    continue;
                }
                let bytes = turn_input_size_bytes(&source.items[index]);
                if initial_input.len().saturating_add(output.items.len())
                    >= self.max_pending_turn_input_items
                    || output.bytes.saturating_add(bytes) > self.max_pending_turn_input_bytes
                {
                    return false;
                }
                output.items.push(source.items.remove(index));
                source.bytes -= bytes;
                output.bytes += bytes;
            }
            true
        };
        if !take_owned_mail(&mut recovered, &mut output) {
            return output.items;
        }
        if let Some(turn) = turn.as_mut()
            && !take_owned_mail(&mut turn.pending_input, &mut output)
        {
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

    #[cfg(test)]
    pub(crate) async fn take_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> Vec<TurnInput> {
        std::mem::take(&mut turn_state.lock().await.pending_input).items
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
        let has_input = !turn_state.pending_input.items.is_empty();
        recovered.append(&mut turn_state.pending_input);
        has_input
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn get_pending_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> Vec<TurnInput> {
        // Acquire every required queue before removing accepted input. Cancellation while
        // waiting for any lock must leave all input available to the next consumer.
        // Match active-turn injection's active -> recovery -> turn-state lock order.
        let active = active_turn.lock().await;
        let mut recovered = self.startup_recovery_items.lock().await;
        let mut turn_state = match active.as_ref() {
            Some(active_turn) => Some(active_turn.turn_state.lock().await),
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
        self.has_pending_mailbox_items().await
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

#[cfg(test)]
thread_local! { static INPUT_SIZE_MEASUREMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn turn_input_size_bytes(input: &TurnInput) -> usize {
    #[cfg(test)]
    INPUT_SIZE_MEASUREMENTS.with(|count| count.set(count.get() + 1));
    match input {
        TurnInput::UserInput { content, client_id } => serialized_size(&(content, client_id)),
        TurnInput::ResponseItem(item) | TurnInput::InternalResponseItem(item) => {
            serialized_size(item)
        }
        TurnInput::InterAgentCommunication(communication) => serialized_size(communication),
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
    fn from_items(items: Vec<TurnInput>) -> Self {
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

    fn iter(&self) -> std::slice::Iter<'_, TurnInput> {
        self.items.iter()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(test)]
    fn front(&self) -> Option<&TurnInput> {
        self.items.first()
    }

    fn has_steering_input(&self) -> bool {
        self.items.iter().any(TurnInput::is_steering_input)
    }
}

impl TurnInput {
    fn is_steering_input(&self) -> bool {
        matches!(self, Self::UserInput { .. } | Self::ResponseItem(_))
    }

    fn requires_turn_continuation(&self) -> bool {
        !matches!(self, Self::InternalResponseItem(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::AgentPath;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn admission_measures_only_new_input_not_the_existing_queue() {
        let queue = InputQueue::new();
        let turn = Mutex::new(TurnState::default());
        let input = TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "large existing input".repeat(1024),
                text_elements: Vec::new(),
            }],
            client_id: None,
        };
        queue
            .extend_pending_input_for_turn_state(&turn, &vec![input.clone(); 32])
            .await
            .unwrap();
        INPUT_SIZE_MEASUREMENTS.set(0);
        queue
            .extend_pending_input_for_turn_state(&turn, std::slice::from_ref(&input))
            .await
            .unwrap();
        assert_eq!(
            INPUT_SIZE_MEASUREMENTS.get(),
            1,
            "admission must not serialize the 32 retained items again"
        );
        assert_eq!(turn.lock().await.pending_input.items.len(), 33);
    }

    fn make_mail(
        author: AgentPath,
        recipient: AgentPath,
        content: &str,
        trigger_turn: bool,
    ) -> InterAgentCommunication {
        InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            content.to_string(),
            trigger_turn,
        )
    }

    #[tokio::test]
    async fn initial_mailbox_respects_capacity_and_leaves_steering_owned() {
        let queue = InputQueue::with_pending_turn_input_limits(3, usize::MAX);
        let turn = ActiveTurn::default();
        let turn_state = Arc::clone(&turn.turn_state);
        let active = Mutex::new(Some(turn));
        let user = |text: &str| TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        };
        let mail = |text| {
            make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").unwrap(),
                text,
                false,
            )
        };
        let recovered = mail("recovered");
        let pending = mail("pending");
        let queued = mail("mailbox");
        queue
            .restore_transferred_startup_input(vec![
                user("recovered user"),
                TurnInput::InterAgentCommunication(recovered.clone()),
            ])
            .await;
        // Seed already-owned inputs to exercise extraction independently of admission.
        turn_state.lock().await.pending_input = TurnInputQueue::from_items(vec![
            user("steer"),
            TurnInput::InterAgentCommunication(pending.clone()),
        ]);
        queue
            .enqueue_mailbox_communication(queued.clone())
            .await
            .unwrap();
        let first = queue
            .take_mailbox_for_initial_input(&active, &[user("fresh")])
            .await;
        assert_eq!(
            first,
            vec![
                TurnInput::InterAgentCommunication(recovered),
                TurnInput::InterAgentCommunication(pending)
            ]
        );
        assert_eq!(
            queue.startup_recovery_items.lock().await.items,
            vec![user("recovered user")]
        );
        assert_eq!(
            turn_state.lock().await.pending_input.items,
            vec![user("steer")]
        );
        assert!(queue.has_pending_mailbox_items().await);
        assert_eq!(
            queue
                .take_mailbox_for_initial_input(&active, &[user("fresh")])
                .await,
            vec![TurnInput::InterAgentCommunication(queued)]
        );
        assert!(!queue.has_pending_mailbox_items().await);
        let recovered_bytes = queue.startup_recovery_items.lock().await.bytes;
        assert_eq!(
            recovered_bytes,
            turn_input_size_bytes(&user("recovered user"))
        );
        assert_eq!(
            turn_state.lock().await.pending_input.bytes,
            turn_input_size_bytes(&user("steer"))
        );
    }

    #[tokio::test]
    async fn input_queue_notifies_mailbox_subscribers() {
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending_activity) = input_queue
            .subscribe_activity(
                /*turn_state*/ None, /*has_internal_completion*/ false,
            )
            .await;
        assert_eq!(pending_activity, None);

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "one",
                /*trigger_turn*/ false,
            ))
            .await
            .expect("mailbox admission");
        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "two",
                /*trigger_turn*/ false,
            ))
            .await
            .expect("mailbox admission");

        activity_rx.changed().await.expect("mailbox update");
        assert_eq!(
            *activity_rx.borrow_and_update(),
            InputQueueActivity::Mailbox
        );
    }

    #[tokio::test]
    async fn input_queue_notifies_steer_subscribers() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let (mut activity_rx, pending_activity) = input_queue
            .subscribe_activity(Some(&turn_state), /*has_internal_completion*/ false)
            .await;
        assert_eq!(pending_activity, None);

        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                &[TurnInput::UserInput {
                    content: vec![UserInput::Text {
                        text: "steer".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
                || {},
            )
            .await
            .expect("steer input should fit");

        activity_rx.changed().await.expect("steer update");
        assert_eq!(*activity_rx.borrow_and_update(), InputQueueActivity::Steer);
    }

    #[tokio::test]
    async fn input_queue_reports_already_pending_steer() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                &[TurnInput::UserInput {
                    content: vec![UserInput::Text {
                        text: "already pending".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
                || {},
            )
            .await
            .expect("steer input should fit");

        let (_activity_rx, pending_activity) = input_queue
            .subscribe_activity(Some(&turn_state), /*has_internal_completion*/ false)
            .await;

        assert_eq!(pending_activity, Some(InputQueueActivity::Steer));
    }

    /// A deferred result is worth ending a quiet wait for, but an agent must
    /// never be told "a job finished" while the user is waiting to steer it.
    #[tokio::test]
    async fn steering_outranks_an_internal_completion_that_arrives_with_it() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());

        // Nothing pending but a completion: it is reported.
        let (_activity_rx, pending_activity) = input_queue
            .subscribe_activity(Some(&turn_state), /*has_internal_completion*/ true)
            .await;
        assert_eq!(
            pending_activity,
            Some(InputQueueActivity::InternalCompletion)
        );

        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                &[TurnInput::UserInput {
                    content: vec![UserInput::Text {
                        text: "steer me".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
                || {},
            )
            .await
            .expect("steer input should fit");

        // Both pending: steering wins, however late the completion's wake was.
        input_queue.publish_internal_completion();
        let (_activity_rx, pending_activity) = input_queue
            .subscribe_activity(Some(&turn_state), /*has_internal_completion*/ true)
            .await;
        assert_eq!(
            pending_activity,
            Some(InputQueueActivity::Steer),
            "a low-priority completion must not mask pending user steering"
        );
    }

    /// Re-deriving from state is what makes the previous guarantee hold for a
    /// waiter that is already parked: the watch carries only its latest value,
    /// so the completion's wake would otherwise overwrite the steer signal.
    #[tokio::test]
    async fn a_parked_waiter_re_derives_steering_after_a_completion_wake() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let (_activity_rx, _) = input_queue
            .subscribe_activity(Some(&turn_state), /*has_internal_completion*/ false)
            .await;

        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                &[TurnInput::UserInput {
                    content: vec![UserInput::Text {
                        text: "steer me".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
                || {},
            )
            .await
            .expect("steer input should fit");
        // The completion publishes last, so the watch's latest value is the
        // low-priority one.
        input_queue.publish_internal_completion();

        assert_eq!(
            input_queue
                .pending_activity(Some(&turn_state), /*has_internal_completion*/ true)
                .await,
            Some(InputQueueActivity::Steer),
            "priority comes from queue state, not from the last value published"
        );
    }

    #[tokio::test]
    async fn input_queue_drains_mailbox_in_delivery_order() {
        let input_queue = InputQueue::new();
        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        let mail_two = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "two",
            /*trigger_turn*/ true,
        );

        input_queue
            .enqueue_mailbox_communication(mail_one.clone())
            .await
            .expect("mailbox admission");
        input_queue
            .enqueue_mailbox_communication(mail_two.clone())
            .await
            .expect("mailbox admission");

        assert_eq!(
            input_queue.get_pending_input(&Mutex::new(None)).await,
            vec![
                TurnInput::InterAgentCommunication(mail_one),
                TurnInput::InterAgentCommunication(mail_two)
            ]
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Hold the contested owner to assert cancellation, admission, or cleanup behavior under contention"
    )]
    async fn pending_mail_checks_observe_a_consistent_recovery_snapshot() {
        for check in 0..3 {
            let input_queue = InputQueue::new();
            let has_work = || async {
                match check {
                    0 => input_queue.has_pending_mailbox_items().await,
                    1 => input_queue.has_trigger_turn_mailbox_items().await,
                    _ => input_queue.has_pending_turn_start_work().await,
                }
            };
            let mail = make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "recovered while checking pending work",
                true,
            );

            let mailbox_guard = input_queue.mailbox.lock().await;
            let mut pending_check = Box::pin(has_work());
            assert!(futures::poll!(pending_check.as_mut()).is_pending());
            let mut recovery = Box::pin(input_queue.restore_transferred_startup_input(vec![
                TurnInput::InterAgentCommunication(mail.clone()),
            ]));
            // Recovery must wait until the check finishes reading both queues.
            assert!(futures::poll!(recovery.as_mut()).is_pending());

            drop(mailbox_guard);
            assert!(!pending_check.await);
            recovery.await;
            assert!(has_work().await);
            assert_eq!(
                input_queue.get_pending_input(&Mutex::new(None)).await,
                vec![TurnInput::InterAgentCommunication(mail)]
            );
            assert!(!has_work().await);
        }
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Hold the contested owner to assert cancellation, admission, or cleanup behavior under contention"
    )]
    async fn cancelled_input_extraction_preserves_all_accepted_queues() {
        let input_queue = InputQueue::new();
        let active_turn = Mutex::new(Some(ActiveTurn::default()));
        let turn_state = Arc::clone(
            &active_turn
                .lock()
                .await
                .as_ref()
                .expect("active turn")
                .turn_state,
        );
        let user_input = |text: &str| TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        };
        let recovered = user_input("recovered");
        let steering = user_input("steering");
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                turn_state.as_ref(),
                std::slice::from_ref(&steering),
                || {},
            )
            .await
            .expect("accept steering");
        input_queue
            .restore_transferred_startup_input(vec![recovered.clone()])
            .await;
        let mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "mailbox",
            false,
        );
        assert!(
            input_queue
                .enqueue_mailbox_communication(mail.clone())
                .await
                .expect("mailbox admission")
        );

        let mailbox_guard = input_queue.mailbox.lock().await;
        let mut extraction = Box::pin(input_queue.get_pending_input(&active_turn));
        assert!(futures::poll!(extraction.as_mut()).is_pending());
        drop(extraction);
        drop(mailbox_guard);

        assert_eq!(
            input_queue.get_pending_input(&active_turn).await,
            vec![
                recovered,
                steering,
                TurnInput::InterAgentCommunication(mail)
            ]
        );
        assert!(input_queue.get_pending_input(&active_turn).await.is_empty());
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Hold the contested owner to assert cancellation, admission, or cleanup behavior under contention"
    )]
    async fn task_start_transfer_preserves_queues_on_cancellation_and_rejects_stale_turns() {
        let input_queue = InputQueue::new();
        let turn = ActiveTurn::default();
        let turn_state = Arc::clone(&turn.turn_state);
        let active_turn = Mutex::new(Some(turn));
        let input = |text: &str| TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        };
        let recovered = input("recovered before startup");
        let steering = input("accepted steering");
        input_queue
            .restore_transferred_startup_input(vec![recovered.clone()])
            .await;
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                turn_state.as_ref(),
                std::slice::from_ref(&steering),
                || {},
            )
            .await
            .expect("accept steering");
        let mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "pending mailbox",
            false,
        );
        assert!(
            input_queue
                .enqueue_mailbox_communication(mail.clone())
                .await
                .expect("mailbox admission")
        );

        let mailbox_guard = input_queue.mailbox.lock().await;
        let mut transfer =
            Box::pin(input_queue.transfer_pending_input_to_turn_state(&active_turn, &turn_state));
        assert!(futures::poll!(transfer.as_mut()).is_pending());
        drop(transfer);
        drop(mailbox_guard);
        assert_eq!(
            turn_state.lock().await.pending_input.items,
            vec![steering.clone()]
        );
        assert_eq!(
            input_queue.startup_recovery_items.lock().await.front(),
            Some(&recovered)
        );
        assert!(input_queue.has_pending_mailbox_items().await);

        let stale_turn = Arc::new(Mutex::new(TurnState::default()));
        assert!(
            !input_queue
                .transfer_pending_input_to_turn_state(&active_turn, &stale_turn)
                .await
        );
        assert!(stale_turn.lock().await.pending_input.items.is_empty());
        assert!(
            input_queue
                .transfer_pending_input_to_turn_state(&active_turn, &turn_state)
                .await
        );
        assert_eq!(
            turn_state.lock().await.pending_input.items,
            vec![
                recovered,
                steering,
                TurnInput::InterAgentCommunication(mail)
            ]
        );
        assert!(input_queue.startup_recovery_items.lock().await.is_empty());
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Hold the contested owner to assert cancellation, admission, or cleanup behavior under contention"
    )]
    async fn cancelled_turn_recovery_preserves_input_and_retry_moves_it_once() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let input = TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "accepted before cancellation".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        };
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, std::slice::from_ref(&input))
            .await
            .expect("accept input");
        let recovery_guard = input_queue.startup_recovery_items.lock().await;
        let mut recovery = Box::pin(input_queue.recover_pending_input_for_turn_state(&turn_state));
        assert!(futures::poll!(recovery.as_mut()).is_pending());
        drop(recovery);
        assert_eq!(
            turn_state.lock().await.pending_input.items,
            vec![input.clone()]
        );
        assert!(recovery_guard.is_empty());
        drop(recovery_guard);

        assert!(
            input_queue
                .recover_pending_input_for_turn_state(&turn_state)
                .await
        );
        assert!(
            !input_queue
                .recover_pending_input_for_turn_state(&turn_state)
                .await
        );
        assert!(turn_state.lock().await.pending_input.items.is_empty());
        let idle = Mutex::new(None);
        assert_eq!(input_queue.get_pending_input(&idle).await, vec![input]);
        assert!(input_queue.get_pending_input(&idle).await.is_empty());
    }

    #[tokio::test]
    async fn input_queue_tracks_pending_trigger_turn_mail() {
        let input_queue = InputQueue::new();

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "queued",
                /*trigger_turn*/ false,
            ))
            .await
            .expect("mailbox admission");
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "wake",
                /*trigger_turn*/ true,
            ))
            .await
            .expect("mailbox admission");
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
    }

    #[tokio::test]
    async fn deterministic_mailbox_ids_are_deduplicated_and_seeded_from_history() {
        let id = codex_protocol::ResponseItemId::from_server("terminal-parent-effect".to_string());
        let mut communication = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "done",
            /*trigger_turn*/ false,
        );
        communication.id = Some(id);

        let input_queue = InputQueue::new();
        assert!(
            input_queue
                .enqueue_mailbox_communication(communication.clone())
                .await
                .expect("mailbox admission")
        );
        assert!(
            !input_queue
                .enqueue_mailbox_communication(communication.clone())
                .await
                .expect("mailbox admission")
        );

        let restored = InputQueue::new();
        restored
            .seed_seen_mailbox_communication_ids(&[RolloutItem::InterAgentCommunication(
                communication.clone(),
            )])
            .await;
        assert!(
            !restored
                .enqueue_mailbox_communication(communication)
                .await
                .expect("mailbox admission")
        );
    }

    #[tokio::test]
    async fn mailbox_admission_is_bounded_without_poisoning_retries() {
        let input_queue =
            InputQueue::with_mailbox_limits(/*max_pending*/ 1, /*max_seen_ids*/ 4);
        let first = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        let mut retry = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "retry",
            /*trigger_turn*/ false,
        );
        retry.id = Some(codex_protocol::ResponseItemId::from_server(
            "bounded-retry".to_string(),
        ));

        assert!(
            input_queue
                .enqueue_mailbox_communication(first)
                .await
                .expect("mailbox admission")
        );
        let error = input_queue
            .enqueue_mailbox_communication(retry.clone())
            .await
            .expect_err("full mailbox must reject admission");
        assert!(
            matches!(error, codex_protocol::error::CodexErr::InvalidRequest(message) if message.contains("session mailbox is full"))
        );
        assert_eq!(
            input_queue.get_pending_input(&Mutex::new(None)).await.len(),
            1
        );
        assert!(
            input_queue
                .enqueue_mailbox_communication(retry)
                .await
                .expect("mailbox admission")
        );
    }

    #[tokio::test]
    async fn seen_mailbox_ids_evict_the_oldest_history_entry() {
        let input_queue =
            InputQueue::with_mailbox_limits(/*max_pending*/ 2, /*max_seen_ids*/ 2);
        let ids = ["mailbox-one", "mailbox-two", "mailbox-three"];
        let history = ids.map(|id| {
            let mut communication = make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                id,
                /*trigger_turn*/ false,
            );
            communication.id = Some(codex_protocol::ResponseItemId::from_server(id.to_string()));
            RolloutItem::InterAgentCommunication(communication)
        });
        input_queue
            .seed_seen_mailbox_communication_ids(&history)
            .await;

        let mut oldest = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "oldest",
            /*trigger_turn*/ false,
        );
        oldest.id = Some(codex_protocol::ResponseItemId::from_server(
            ids[0].to_string(),
        ));
        let mut newest = oldest.clone();
        newest.id = Some(codex_protocol::ResponseItemId::from_server(
            ids[2].to_string(),
        ));

        assert!(
            input_queue
                .enqueue_mailbox_communication(oldest)
                .await
                .expect("mailbox admission")
        );
        assert!(
            !input_queue
                .enqueue_mailbox_communication(newest)
                .await
                .expect("mailbox admission")
        );
    }

    #[tokio::test]
    async fn pending_turn_input_item_admission_is_bounded_across_recovery() {
        let input_queue = InputQueue::with_pending_turn_input_limits(1, usize::MAX);
        let turn_state = Mutex::new(TurnState::default());
        let input = vec![TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "first".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        }];

        input_queue
            .extend_pending_input_for_turn_state(&turn_state, &input)
            .await
            .expect("first item should fit");
        assert_eq!(
            input_queue
                .extend_pending_input_for_turn_state(&turn_state, &input)
                .await,
            Err(PendingInputAdmissionError {
                max_items: 1,
                max_bytes: usize::MAX,
            })
        );

        let recovered = input_queue
            .take_pending_input_for_turn_state(&turn_state)
            .await;
        input_queue
            .restore_transferred_startup_input(recovered)
            .await;
        let next_turn_state = Mutex::new(TurnState::default());
        assert_eq!(
            input_queue
                .extend_pending_input_for_turn_state(&next_turn_state, &input)
                .await,
            Err(PendingInputAdmissionError {
                max_items: 1,
                max_bytes: usize::MAX,
            })
        );
    }

    #[tokio::test]
    async fn recovered_user_input_is_pending_turn_start_work() {
        let input_queue = InputQueue::new();
        input_queue
            .restore_transferred_startup_input(vec![TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text: "continue in a fresh turn".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            }])
            .await;

        assert!(input_queue.has_pending_turn_start_work().await);
    }

    #[tokio::test]
    async fn recovered_input_activity_preserves_steering_and_ignores_internal_context() {
        let user = TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "change direction".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        };
        let mail = |trigger_turn| {
            TurnInput::InterAgentCommunication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "agent update",
                trigger_turn,
            ))
        };
        for (input, expected) in [
            (vec![user.clone()], Some(InputQueueActivity::Steer)),
            (
                vec![mail(true), user.clone()],
                Some(InputQueueActivity::Steer),
            ),
            (vec![mail(true)], Some(InputQueueActivity::Mailbox)),
            (vec![mail(false)], Some(InputQueueActivity::Mailbox)),
            (
                vec![TurnInput::InternalResponseItem(ResponseItem::Other)],
                None,
            ),
        ] {
            let queue = InputQueue::new();
            let (mut receiver, pending) = queue
                .subscribe_activity(None, /*has_internal_completion*/ false)
                .await;
            assert_eq!(pending, None);
            queue.restore_transferred_startup_input(input.clone()).await;
            assert_eq!(receiver.has_changed().unwrap(), expected.is_some());
            if let Some(expected) = expected {
                assert_eq!(*receiver.borrow_and_update(), expected);
            }
            let (_, pending) = queue
                .subscribe_activity(None, /*has_internal_completion*/ false)
                .await;
            assert_eq!(
                pending, expected,
                "late subscribers must see recovered work"
            );
            assert_eq!(queue.get_pending_input(&Mutex::new(None)).await, input);
            let (_, pending) = queue
                .subscribe_activity(None, /*has_internal_completion*/ false)
                .await;
            assert_eq!(pending, None);
        }

        let queue = InputQueue::new();
        queue
            .restore_transferred_startup_input(vec![user.clone()])
            .await;
        let (mut receiver, _) = queue
            .subscribe_activity(None, /*has_internal_completion*/ false)
            .await;
        let later_mail = mail(true);
        queue
            .restore_transferred_startup_input(vec![later_mail.clone()])
            .await;
        assert!(receiver.has_changed().unwrap());
        assert_eq!(*receiver.borrow_and_update(), InputQueueActivity::Steer);
        assert_eq!(
            queue.get_pending_input(&Mutex::new(None)).await,
            vec![later_mail, user]
        );
    }

    #[tokio::test]
    async fn pending_turn_input_byte_admission_is_bounded() {
        let input = TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "bounded bytes".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: None,
        };
        let input_size = turn_input_size_bytes(&input);
        let input_queue = InputQueue::with_pending_turn_input_limits(2, input_size);
        let turn_state = Mutex::new(TurnState::default());

        input_queue
            .extend_pending_input_for_turn_state(&turn_state, std::slice::from_ref(&input))
            .await
            .expect("first item should fit the exact byte budget");
        assert_eq!(
            input_queue
                .extend_pending_input_for_turn_state(&turn_state, &[input])
                .await,
            Err(PendingInputAdmissionError {
                max_items: 2,
                max_bytes: input_size,
            })
        );
    }

    #[tokio::test]
    async fn mailbox_bytes_bound_admission_and_fifo_transfer_without_poisoning_retry() {
        let mail = |text: &str| make_mail(AgentPath::root(), AgentPath::root(), text, false);
        let first = mail("first");
        let mut second = mail("second");
        second.id = Some(codex_protocol::ResponseItemId::from_server(
            "retry-byte-limit".to_string(),
        ));
        let budget = serialized_size(&first) + serialized_size(&second);
        let queue = InputQueue::with_pending_turn_input_limits(10, budget);
        let turn = ActiveTurn::default();
        let turn_state = Arc::clone(&turn.turn_state);
        let active = Mutex::new(Some(turn));
        let steering = TurnInput::ResponseItem(ResponseItem::Other);
        queue
            .extend_pending_input_for_turn_state(&turn_state, std::slice::from_ref(&steering))
            .await
            .unwrap();
        queue
            .enqueue_mailbox_communication(first.clone())
            .await
            .unwrap();
        queue
            .enqueue_mailbox_communication(second.clone())
            .await
            .unwrap();
        let mut retry = mail("retry");
        retry.id = Some(codex_protocol::ResponseItemId::from_server(
            "rejected-mail".to_string(),
        ));
        assert!(
            queue
                .enqueue_mailbox_communication(retry.clone())
                .await
                .is_err()
        );
        assert!(
            queue
                .transfer_pending_input_to_turn_state(&active, &turn_state)
                .await
        );
        {
            let state = turn_state.lock().await;
            assert_eq!(
                state.pending_input.items,
                vec![
                    steering.clone(),
                    TurnInput::InterAgentCommunication(first.clone())
                ]
            );
            assert_eq!(
                state.pending_input.bytes,
                state
                    .pending_input
                    .items
                    .iter()
                    .map(turn_input_size_bytes)
                    .sum::<usize>()
            );
        }
        assert_eq!(queue.mailbox.lock().await.bytes, serialized_size(&second));
        assert_eq!(
            queue.get_pending_input(&active).await,
            vec![steering, TurnInput::InterAgentCommunication(first)]
        );
        assert_eq!(
            queue.get_pending_input(&active).await,
            vec![TurnInput::InterAgentCommunication(second)]
        );
        assert_eq!(queue.mailbox.lock().await.bytes, 0);
        assert!(queue.enqueue_mailbox_communication(retry).await.unwrap());
        assert!(
            queue
                .enqueue_mailbox_communication(mail(&"x".repeat(budget)))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn pending_bytes_follow_recovery_transfer_drain_and_clear() {
        let queue = InputQueue::new();
        let turn = ActiveTurn::default();
        let turn_state = Arc::clone(&turn.turn_state);
        let active = Mutex::new(Some(turn));
        let input = TurnInput::UserInput {
            content: vec![UserInput::Text {
                text: "escaped \"quotes\"\n\\ unicode cafÃ©".to_string(),
                text_elements: Vec::new(),
            }],
            client_id: Some("client".to_string()),
        };
        let expected = match &input {
            TurnInput::UserInput { content, client_id } => {
                serde_json::to_vec(&(content, client_id)).unwrap().len()
            }
            _ => unreachable!(),
        };
        queue
            .extend_pending_input_for_turn_state(&turn_state, std::slice::from_ref(&input))
            .await
            .unwrap();
        assert_eq!(turn_state.lock().await.pending_input.bytes, expected);
        queue
            .recover_pending_input_for_turn_state(&turn_state)
            .await;
        assert_eq!(turn_state.lock().await.pending_input.bytes, 0);
        assert_eq!(queue.startup_recovery_items.lock().await.bytes, expected);
        assert!(
            queue
                .transfer_pending_input_to_turn_state(&active, &turn_state)
                .await
        );
        assert_eq!(turn_state.lock().await.pending_input.bytes, expected);
        assert_eq!(queue.startup_recovery_items.lock().await.bytes, 0);
        assert_eq!(queue.get_pending_input(&active).await, vec![input.clone()]);
        assert_eq!(turn_state.lock().await.pending_input.bytes, 0);
        queue
            .extend_pending_input_for_turn_state(&turn_state, &[input])
            .await
            .unwrap();
        queue.clear_pending_for_turn_state(&turn_state).await;
        assert_eq!(turn_state.lock().await.pending_input.bytes, 0);
    }
}
