use crate::ServerNotification;

/// Backpressure behavior shared by every app-server event delivery layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerNotificationDeliveryClass {
    /// Control and lifecycle state must be delivered in order.
    Reliable,
    /// Text can be concatenated with an adjacent delta for the same stream.
    CoalescibleDelta,
    /// A newer authoritative event makes this progress-only update dispensable.
    BestEffort,
}

impl ServerNotification {
    /// Returns the single delivery policy used by app-server, in-process clients,
    /// and the TUI.
    pub fn delivery_class(&self) -> ServerNotificationDeliveryClass {
        match self {
            Self::AgentMessageDelta(_)
            | Self::PlanDelta(_)
            | Self::ReasoningSummaryTextDelta(_)
            | Self::ReasoningTextDelta(_)
            | Self::CommandExecutionOutputDelta(_)
            | Self::FileChangeOutputDelta(_) => {
                ServerNotificationDeliveryClass::CoalescibleDelta
            }
            Self::ThreadTokenUsageUpdated(_)
            | Self::McpToolCallProgress(_)
            | Self::ExternalAgentConfigImportProgress(_)
            | Self::FuzzyFileSearchSessionUpdated(_) => {
                ServerNotificationDeliveryClass::BestEffort
            }
            _ => ServerNotificationDeliveryClass::Reliable,
        }
    }

    /// Concatenates `newer` into this notification only when both values are
    /// adjacent chunks from exactly the same textual stream.
    pub fn try_coalesce_delta(&mut self, newer: Self) -> Result<(), Self> {
        let same_stream = match (&*self, &newer) {
            (Self::AgentMessageDelta(current), Self::AgentMessageDelta(next)) => {
                current.thread_id == next.thread_id
                    && current.turn_id == next.turn_id
                    && current.item_id == next.item_id
            }
            (Self::PlanDelta(current), Self::PlanDelta(next)) => {
                current.thread_id == next.thread_id
                    && current.turn_id == next.turn_id
                    && current.item_id == next.item_id
            }
            (
                Self::ReasoningSummaryTextDelta(current),
                Self::ReasoningSummaryTextDelta(next),
            ) => {
                current.thread_id == next.thread_id
                    && current.turn_id == next.turn_id
                    && current.item_id == next.item_id
                    && current.summary_index == next.summary_index
            }
            (Self::ReasoningTextDelta(current), Self::ReasoningTextDelta(next)) => {
                current.thread_id == next.thread_id
                    && current.turn_id == next.turn_id
                    && current.item_id == next.item_id
                    && current.content_index == next.content_index
            }
            (
                Self::CommandExecutionOutputDelta(current),
                Self::CommandExecutionOutputDelta(next),
            ) => {
                current.thread_id == next.thread_id
                    && current.turn_id == next.turn_id
                    && current.item_id == next.item_id
            }
            (Self::FileChangeOutputDelta(current), Self::FileChangeOutputDelta(next)) => {
                current.thread_id == next.thread_id
                    && current.turn_id == next.turn_id
                    && current.item_id == next.item_id
            }
            _ => false,
        };
        if !same_stream {
            return Err(newer);
        }

        match (self, newer) {
            (Self::AgentMessageDelta(current), Self::AgentMessageDelta(next)) => {
                current.delta.push_str(&next.delta);
            }
            (Self::PlanDelta(current), Self::PlanDelta(next)) => {
                current.delta.push_str(&next.delta);
            }
            (
                Self::ReasoningSummaryTextDelta(current),
                Self::ReasoningSummaryTextDelta(next),
            ) => {
                current.delta.push_str(&next.delta);
            }
            (Self::ReasoningTextDelta(current), Self::ReasoningTextDelta(next)) => {
                current.delta.push_str(&next.delta);
            }
            (
                Self::CommandExecutionOutputDelta(current),
                Self::CommandExecutionOutputDelta(next),
            ) => {
                current.delta.push_str(&next.delta);
            }
            (Self::FileChangeOutputDelta(current), Self::FileChangeOutputDelta(next)) => {
                current.delta.push_str(&next.delta);
            }
            _ => unreachable!("same-stream classification and append variants must agree"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMessageDeltaNotification;
    use crate::McpToolCallProgressNotification;

    fn assistant_delta(item_id: &str, delta: &str) -> ServerNotification {
        ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: item_id.to_string(),
            delta: delta.to_string(),
        })
    }

    #[test]
    fn adjacent_same_stream_assistant_deltas_coalesce_losslessly() {
        let mut current = assistant_delta("item-1", "hello ");
        current
            .try_coalesce_delta(assistant_delta("item-1", "world"))
            .expect("same stream should coalesce");

        let ServerNotification::AgentMessageDelta(notification) = current else {
            panic!("expected assistant delta");
        };
        assert_eq!(notification.delta, "hello world");
    }

    #[test]
    fn different_assistant_streams_do_not_coalesce() {
        let mut current = assistant_delta("item-1", "one");
        assert!(
            current
                .try_coalesce_delta(assistant_delta("item-2", "two"))
                .is_err()
        );
    }

    #[test]
    fn progress_is_best_effort_but_assistant_text_is_not() {
        assert_eq!(
            assistant_delta("item-1", "text").delivery_class(),
            ServerNotificationDeliveryClass::CoalescibleDelta
        );
        assert_eq!(
            ServerNotification::McpToolCallProgress(McpToolCallProgressNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                message: "working".to_string(),
            })
            .delivery_class(),
            ServerNotificationDeliveryClass::BestEffort
        );
    }
}
