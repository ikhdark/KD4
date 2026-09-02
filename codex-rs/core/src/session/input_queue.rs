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
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
    startup_recovery_items: Mutex<VecDeque<TurnInput>>,
    mailbox: Mutex<MailboxState>,
    max_pending_mailbox_communications: usize,
    max_seen_mailbox_communication_ids: usize,
    max_pending_turn_input_items: usize,
    max_pending_turn_input_bytes: usize,
}

#[derive(Default)]
struct MailboxState {
    pending_mails: VecDeque<InterAgentCommunication>,
    seen_communication_ids: HashSet<codex_protocol::ResponseItemId>,
    seen_communication_id_order: VecDeque<codex_protocol::ResponseItemId>,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            startup_recovery_items: Mutex::new(VecDeque::new()),
            mailbox: Mutex::new(MailboxState::default()),
            max_pending_mailbox_communications: MAX_PENDING_MAILBOX_COMMUNICATIONS,
            max_seen_mailbox_communication_ids: MAX_SEEN_MAILBOX_COMMUNICATION_IDS,
            max_pending_turn_input_items: MAX_PENDING_TURN_INPUT_ITEMS,
            max_pending_turn_input_bytes: MAX_PENDING_TURN_INPUT_BYTES,
        }
    }

    #[cfg(test)]
    fn with_mailbox_limits(max_pending: usize, max_seen_ids: usize) -> Self {
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

    pub(crate) async fn subscribe_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
    ) -> (
        watch::Receiver<InputQueueActivity>,
        Option<InputQueueActivity>,
    ) {
        let activity_rx = self.activity_tx.subscribe();
        let has_pending_steer = if let Some(turn_state) = turn_state {
            turn_state.lock().await.pending_input.has_steering_input()
        } else {
            false
        };
        let pending_activity = if has_pending_steer {
            Some(InputQueueActivity::Steer)
        } else if self.has_pending_mailbox_items().await {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        };
        (activity_rx, pending_activity)
    }

    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
    ) -> bool {
        let mut mailbox = self.mailbox.lock().await;
        if communication
            .id
            .as_ref()
            .is_some_and(|id| mailbox.seen_communication_ids.contains(id))
        {
            return false;
        }
        if mailbox.pending_mails.len() >= self.max_pending_mailbox_communications {
            tracing::warn!(
                max_pending = self.max_pending_mailbox_communications,
                "rejecting mailbox communication because the session mailbox is full"
            );
            return false;
        }
        if let Some(id) = communication.id.as_ref() {
            mailbox.seen_communication_ids.insert(id.clone());
            mailbox.seen_communication_id_order.push_back(id.clone());
            compact_seen_mailbox_ids(&mut mailbox, self.max_seen_mailbox_communication_ids);
        }
        mailbox.pending_mails.push_back(communication);
        drop(mailbox);
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        true
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

    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        self.startup_recovery_items
            .lock()
            .await
            .iter()
            .any(|item| matches!(item, TurnInput::InterAgentCommunication(_)))
            || !self.mailbox.lock().await.pending_mails.is_empty()
    }

    pub(crate) async fn has_trigger_turn_mailbox_items(&self) -> bool {
        self.startup_recovery_items.lock().await.iter().any(
            |item| matches!(item, TurnInput::InterAgentCommunication(mail) if mail.trigger_turn),
        ) || self
            .mailbox
            .lock()
            .await
            .pending_mails
            .iter()
            .any(|mail| mail.trigger_turn)
    }

    /// Whether recovered input should start a new turn once the current turn releases its
    /// terminal fence. User input was already accepted as turn work, while mailbox input still
    /// honors its explicit `trigger_turn` policy.
    pub(crate) async fn has_pending_turn_start_work(&self) -> bool {
        self.startup_recovery_items.lock().await.iter().any(|item| {
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
            .any(|mail| mail.trigger_turn)
    }

    /// Restores already-admitted input owned by a taskless startup placeholder
    /// that was cancelled before a supervisor could take ownership. Restored
    /// items precede work accepted after the cancellation.
    pub(crate) async fn restore_transferred_startup_input(&self, input: Vec<TurnInput>) {
        if input.is_empty() {
            return;
        }
        let activity = if input.iter().any(
            |item| matches!(item, TurnInput::InterAgentCommunication(mail) if mail.trigger_turn),
        ) {
            InputQueueActivity::Mailbox
        } else {
            InputQueueActivity::Steer
        };
        let mut recovered = self.startup_recovery_items.lock().await;
        let mut restored = VecDeque::from(input);
        restored.append(&mut recovered);
        *recovered = restored;
        drop(recovered);
        self.activity_tx.send_replace(activity);
    }

    pub(crate) async fn drain_mailbox_input_items(&self) -> Vec<TurnInput> {
        self.mailbox
            .lock()
            .await
            .pending_mails
            .drain(..)
            .map(TurnInput::InterAgentCommunication)
            .collect()
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
        turn_state.pending_input.items.clear();
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
        if !turn_state.pending_input.items.is_empty() {
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
    ) -> Result<(), PendingInputAdmissionError> {
        {
            let recovered = self.startup_recovery_items.lock().await;
            let mut turn_state = turn_state.lock().await;
            self.check_pending_turn_input_capacity(
                recovered.iter(),
                turn_state.pending_input.items.iter(),
                input,
            )?;
            turn_state.pending_input.items.extend_from_slice(input);
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
        let recovered = self.startup_recovery_items.lock().await;
        let mut turn_state = turn_state.lock().await;
        self.check_pending_turn_input_capacity(
            recovered.iter(),
            turn_state.pending_input.items.iter(),
            input,
        )?;
        turn_state.pending_input.items.extend_from_slice(input);
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

    pub(crate) async fn restore_transferred_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        // This is an ownership transfer from the session's recovery, active,
        // and bounded mailbox queues, not a new external admission.
        turn_state.lock().await.pending_input.items.extend(input);
    }

    fn check_pending_turn_input_capacity<'a>(
        &self,
        recovered: impl Iterator<Item = &'a TurnInput>,
        active: impl Iterator<Item = &'a TurnInput>,
        input: &[TurnInput],
    ) -> Result<(), PendingInputAdmissionError> {
        let mut item_count = input.len();
        let mut byte_count = input.iter().fold(0usize, |total, item| {
            total.saturating_add(turn_input_size_bytes(item))
        });
        for item in recovered.chain(active) {
            item_count = item_count.saturating_add(1);
            byte_count = byte_count.saturating_add(turn_input_size_bytes(item));
        }
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

    pub(crate) async fn take_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> Vec<TurnInput> {
        turn_state.lock().await.pending_input.items.split_off(0)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn get_pending_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> Vec<TurnInput> {
        let recovered_input: Vec<TurnInput> =
            self.startup_recovery_items.lock().await.drain(..).collect();
        let (pending_input, accepts_mailbox_delivery) = {
            let mut active = active_turn.lock().await;
            match active.as_mut() {
                Some(active_turn) => {
                    let mut turn_state = active_turn.turn_state.lock().await;
                    (
                        turn_state.pending_input.items.split_off(0),
                        turn_state.accepts_mailbox_delivery_for_current_turn(),
                    )
                }
                None => (Vec::new(), true),
            }
        };
        if !accepts_mailbox_delivery {
            let mut input = recovered_input;
            input.extend(pending_input);
            return input;
        }
        let mailbox_items = self.drain_mailbox_input_items().await.into_iter();
        let mut input = recovered_input;
        input.extend(pending_input);
        input.extend(mailbox_items);
        input
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

fn turn_input_size_bytes(input: &TurnInput) -> usize {
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
    fn has_steering_input(&self) -> bool {
        self.items.iter().any(|input| {
            matches!(
                input,
                TurnInput::UserInput { .. } | TurnInput::ResponseItem(_)
            )
        })
    }
}

impl TurnInput {
    fn requires_turn_continuation(&self) -> bool {
        !matches!(self, Self::InternalResponseItem(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::AgentPath;
    use pretty_assertions::assert_eq;

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
    async fn input_queue_notifies_mailbox_subscribers() {
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending_activity, None);

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "one",
                /*trigger_turn*/ false,
            ))
            .await;
        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "two",
                /*trigger_turn*/ false,
            ))
            .await;

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
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;
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
            )
            .await
            .expect("steer input should fit");

        let (_activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;

        assert_eq!(pending_activity, Some(InputQueueActivity::Steer));
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
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_two.clone())
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            vec![
                TurnInput::InterAgentCommunication(mail_one),
                TurnInput::InterAgentCommunication(mail_two)
            ]
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
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
            .await;
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "wake",
                /*trigger_turn*/ true,
            ))
            .await;
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
        );
        assert!(
            !input_queue
                .enqueue_mailbox_communication(communication.clone())
                .await
        );

        let restored = InputQueue::new();
        restored
            .seed_seen_mailbox_communication_ids(&[RolloutItem::InterAgentCommunication(
                communication.clone(),
            )])
            .await;
        assert!(!restored.enqueue_mailbox_communication(communication).await);
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

        assert!(input_queue.enqueue_mailbox_communication(first).await);
        assert!(
            !input_queue
                .enqueue_mailbox_communication(retry.clone())
                .await
        );
        assert_eq!(input_queue.drain_mailbox_input_items().await.len(), 1);
        assert!(input_queue.enqueue_mailbox_communication(retry).await);
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

        assert!(input_queue.enqueue_mailbox_communication(oldest).await);
        assert!(!input_queue.enqueue_mailbox_communication(newest).await);
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
}
