use std::collections::HashSet;

use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::models::MessagePhase;

/// Current-turn delivery evidence, not a second history store. Only identities
/// and the last authoritative final item are retained. Overflow and any lag
/// conservatively keep the existing history recovery path.
#[derive(Default)]
pub(crate) struct ExecRecoveryState {
    pending: HashSet<String>,
    completed: HashSet<String>,
    final_item: Option<ThreadItem>,
    uncertain: bool,
}

impl ExecRecoveryState {
    pub(crate) fn mark_gap(&mut self) {
        self.uncertain = true;
    }

    /// Caller filters thread/turn identity before recording delivery evidence.
    pub(crate) fn observe(&mut self, notification: &ServerNotification) {
        const MAX_ITEMS: usize = 4096;
        match notification {
            ServerNotification::ItemStarted(event) => {
                if event.item.id().len() > 512 {
                    self.mark_gap();
                    return;
                }
                if self.completed.contains(event.item.id()) {
                    return; // replay must not reopen a settled item
                }
                if self.pending.len() + self.completed.len() >= MAX_ITEMS {
                    self.mark_gap();
                } else {
                    self.pending.insert(event.item.id().to_string());
                }
            }
            ServerNotification::ItemCompleted(event) => {
                if event.item.id().len() > 512 {
                    self.mark_gap();
                    return;
                }
                if matches!(
                    &event.item,
                    ThreadItem::AgentMessage { .. } | ThreadItem::Plan { .. }
                ) && self.completed.contains(event.item.id())
                {
                    return; // immutable text replay cannot replace a newer final
                }
                if crate::event_processor::item_is_in_progress(&event.item) {
                    self.mark_gap();
                    return; // only terminal evidence closes a started identity
                }
                self.pending.remove(event.item.id());
                if self.completed.len() >= MAX_ITEMS {
                    self.mark_gap();
                } else {
                    self.completed.insert(event.item.id().to_string());
                }
                if matches!(
                    &event.item,
                    ThreadItem::AgentMessage {
                        phase: Some(MessagePhase::FinalAnswer),
                        ..
                    } | ThreadItem::Plan { .. }
                ) {
                    self.final_item = Some(event.item.clone());
                }
            }
            _ => {}
        }
    }

    /// Supply the already-delivered final item only when all started items
    /// closed without a delivery gap. This removes a full-history read, not
    /// semantic synthesis or missing-item reconciliation.
    pub(crate) fn backfill_complete_stream(&self, notification: &mut ServerNotification) {
        let ServerNotification::TurnCompleted(event) = notification else {
            return;
        };
        if self.uncertain
            || !self.pending.is_empty()
            || !event.turn.items.is_empty()
            || event.turn.status != TurnStatus::Completed
        {
            return;
        }
        let Some(item) = self.final_item.as_ref() else {
            return;
        };
        if let Some(answer) = event
            .timing
            .as_ref()
            .or(event.turn.timing.as_ref())
            .and_then(|timing| timing.final_answer.as_ref())
        {
            let Some(message) = answer.message.as_ref() else {
                return;
            };
            let text = match item {
                ThreadItem::AgentMessage { text, .. } | ThreadItem::Plan { text, .. } => text,
                _ => return,
            };
            if message.item_id != item.id() || message.text != *text {
                return;
            }
        }
        event.turn.items.push(item.clone());
        // The subset is sufficient for this consumer, not full server history.
        // Leave items_view unchanged rather than advertising a complete view.
    }
}

