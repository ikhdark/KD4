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
                    ThreadItem::CommandExecution {
                        status: codex_app_server_protocol::CommandExecutionStatus::InProgress,
                        ..
                    } | ThreadItem::McpToolCall {
                        status: codex_app_server_protocol::McpToolCallStatus::InProgress,
                        ..
                    } | ThreadItem::FileChange {
                        status: codex_app_server_protocol::PatchApplyStatus::InProgress,
                        ..
                    } | ThreadItem::DynamicToolCall {
                        status: codex_app_server_protocol::DynamicToolCallStatus::InProgress,
                        ..
                    } | ThreadItem::CollabAgentToolCall {
                        status: codex_app_server_protocol::CollabAgentToolCallStatus::InProgress,
                        ..
                    }
                ) {
                    self.mark_gap();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn completed_message(id: &str) -> ServerNotification {
        ServerNotification::ItemCompleted(codex_app_server_protocol::ItemCompletedNotification {
            thread_id: "thread-1".into(),
            turn_id: "turn-1".into(),
            completed_at_ms: 0,
            item: ThreadItem::AgentMessage {
                id: id.into(),
                text: "{\"answer\":42}".into(),
                phase: Some(MessagePhase::FinalAnswer),
            },
        })
    }

    #[tokio::test]
    async fn healthy_complete_stream_never_polls_history_and_preserves_final_bytes() {
        let mut state = ExecRecoveryState::default();
        let final_item = completed_message("final");
        state.observe(&final_item);
        let mut terminal = crate::tests::recovery_completion();
        state.backfill_complete_stream(&mut terminal);
        let reads = std::cell::Cell::new(0);
        let prepared = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            crate::prepare_server_notification(
                false,
                "thread-1",
                "turn-1",
                true,
                &mut terminal,
                async |_| {
                    reads.set(reads.get() + 1);
                    std::future::pending().await
                },
            ),
        )
        .await
        .expect("a healthy final must not wait for history")
        .unwrap();
        assert!(prepared);
        assert_eq!(reads.get(), 0);
        let ServerNotification::TurnCompleted(event) = terminal else {
            unreachable!()
        };
        assert_eq!(
            crate::event_processor::final_message_from_turn_items(&event.turn.items).as_deref(),
            Some("{\"answer\":42}")
        );
    }

    #[tokio::test]
    async fn gaps_and_unfinished_items_still_require_history() {
        for gap in [false, true] {
            let mut state = ExecRecoveryState::default();
            state.observe(&completed_message("final"));
            if gap {
                state.mark_gap();
            } else {
                state.observe(&ServerNotification::ItemStarted(
                    codex_app_server_protocol::ItemStartedNotification {
                        thread_id: "thread-1".into(),
                        turn_id: "turn-1".into(),
                        started_at_ms: 0,
                        item: ThreadItem::Plan {
                            id: "unfinished".into(),
                            text: String::new(),
                        },
                    },
                ));
            }
            let mut terminal = crate::tests::recovery_completion();
            state.backfill_complete_stream(&mut terminal);
            let reads = std::cell::Cell::new(0);
            crate::prepare_server_notification(
                false,
                "thread-1",
                "turn-1",
                true,
                &mut terminal,
                async |_| {
                    reads.set(reads.get() + 1);
                    Err("deliberate read failure".into())
                },
            )
            .await
            .unwrap();
            assert_eq!(reads.get(), 1, "gap={gap}");
        }
    }

    #[test]
    fn completed_replay_does_not_reopen_items_and_overflow_disables_skip() {
        let mut state = ExecRecoveryState::default();
        state.observe(&completed_message("final"));
        let ServerNotification::ItemCompleted(event) = completed_message("final") else {
            unreachable!()
        };
        state.observe(&ServerNotification::ItemStarted(
            codex_app_server_protocol::ItemStartedNotification {
                thread_id: event.thread_id,
                turn_id: event.turn_id,
                item: event.item,
                started_at_ms: 0,
            },
        ));
        assert!(state.pending.is_empty());
        for index in 0..4100 {
            state.observe(&completed_message(&format!("item-{index}")));
        }
        assert!(state.uncertain);
        assert_eq!(state.completed.len(), 4096);
    }
}
