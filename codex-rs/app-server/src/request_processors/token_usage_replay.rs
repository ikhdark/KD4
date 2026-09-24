//! Replays persisted token usage snapshots when a client attaches to an existing thread.
//!
//! The message processor decides when replay is allowed and preserves JSON-RPC response
//! ordering. This module owns notification construction and the attribution rules that
//! map the latest persisted `TokenCount` back to a v2 turn id.
//!
//! Rollout histories can contain explicit turn ids or generated turn ids. When explicit
//! ids do not match the rebuilt thread, replay falls back to the active turn position at
//! the time the `TokenCount` was persisted so the notification still targets the
//! corresponding rebuilt turn.

use std::sync::Arc;

use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::ThreadTokenUsage;
use codex_app_server_protocol::ThreadTokenUsageUpdatedNotification;
use codex_app_server_protocol::Turn;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TokenUsageInfo;
use codex_rollout::is_persisted_rollout_item;

use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;

/// Sends a restored token usage update to the connection that attached to a thread.
///
/// This is lifecycle replay rather than a model event: the rollout already contains
/// the original `TokenCount`, and emitting through `send_event` here would duplicate
/// persisted usage records. Keeping replay connection-scoped also avoids
/// surprising other subscribers with a historical usage update while they may be
/// rendering live turn events.
pub(super) async fn send_thread_token_usage_update_to_connection(
    outgoing: &Arc<OutgoingMessageSender>,
    connection_id: ConnectionId,
    thread_id: ThreadId,
    replay: TokenUsageReplaySnapshot,
) {
    let notification = ThreadTokenUsageUpdatedNotification {
        thread_id: thread_id.to_string(),
        turn_id: replay.turn_id,
        token_usage: ThreadTokenUsage::from(replay.info),
    };
    outgoing
        .send_initial_component_notification_to_connection(
            connection_id,
            ServerNotification::ThreadTokenUsageUpdated(notification),
        )
        .await;
}

/// Identifies the turn that was active when a `TokenCount` record appeared.
///
/// The id is preferred when it still appears in the rebuilt thread. The position is a
/// fallback for histories whose implicit turn ids are regenerated during reconstruction.
struct TokenUsageTurnOwner {
    id: String,
    position: Option<usize>,
}

pub(super) struct TokenUsageReplaySnapshot {
    pub(super) turn_id: String,
    pub(super) info: TokenUsageInfo,
}

#[derive(Default)]
pub(super) struct TokenUsageReplay {
    turn_owner: Option<TokenUsageTurnOwner>,
    info: Option<TokenUsageInfo>,
}

impl TokenUsageReplay {
    fn observe_rollout_item(&mut self, builder: &ThreadHistoryBuilder, item: &RolloutItem) {
        if let RolloutItem::EventMsg(EventMsg::TokenCount(event)) = item
            && let Some(info) = &event.info
        {
            self.info = Some(info.clone());
            self.turn_owner = builder.active_turn_id().map(|id| TokenUsageTurnOwner {
                id: id.to_string(),
                position: builder.active_turn_position(),
            });
        }
    }

    pub(super) fn into_snapshot(self, turns: &[Turn]) -> Option<TokenUsageReplaySnapshot> {
        Some(TokenUsageReplaySnapshot {
            turn_id: self.turn_owner?.resolve(turns)?,
            info: self.info?,
        })
    }
}

impl TokenUsageTurnOwner {
    fn resolve(self, turns: &[Turn]) -> Option<String> {
        let positional_turn = self.position.and_then(|position| turns.get(position));
        if positional_turn.is_some_and(|turn| turn.id == self.id) {
            return Some(self.id);
        }
        if turns.iter().any(|turn| turn.id == self.id) {
            return Some(self.id);
        }
        positional_turn.map(|turn| turn.id.clone())
    }
}

pub(super) fn build_turns_with_token_usage_replay(
    rollout_items: &[RolloutItem],
) -> (Vec<Turn>, TokenUsageReplay) {
    let mut builder = ThreadHistoryBuilder::new();
    let mut token_usage_replay = TokenUsageReplay::default();

    for item in rollout_items {
        if is_persisted_rollout_item(item, ThreadHistoryMode::Legacy) {
            token_usage_replay.observe_rollout_item(&builder, item);
            builder.handle_rollout_item(item);
        }
    }

    (builder.finish(), token_usage_replay)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::AgentMessageEvent;
    use codex_protocol::protocol::TokenCountEvent;
    use codex_protocol::protocol::TokenUsage;
    use codex_protocol::protocol::UserMessageEvent;
    use pretty_assertions::assert_eq;

    #[test]
    fn replay_attribution_uses_already_loaded_history() {
        let rollout_items = token_usage_history();
        let (turns, replay) = build_turns_with_token_usage_replay(&rollout_items);

        let snapshot = replay.into_snapshot(&turns).expect("usage snapshot");
        assert_eq!(snapshot.turn_id, turns[0].id);
        assert_eq!(snapshot.info.total_token_usage.total_tokens, 150);
        assert_eq!(snapshot.info.last_token_usage.total_tokens, 90);
    }

    #[test]
    fn replay_attribution_falls_back_to_rebuilt_turn_position() {
        let rollout_items = token_usage_history();
        let (mut turns, replay) = build_turns_with_token_usage_replay(&rollout_items);
        turns[0].id = "rebuilt-turn-id".to_string();

        assert_eq!(
            replay
                .into_snapshot(&turns)
                .expect("usage snapshot")
                .turn_id,
            "rebuilt-turn-id"
        );
    }

    #[test]
    fn replay_without_an_attributable_turn_is_suppressed() {
        let items = vec![RolloutItem::EventMsg(EventMsg::TokenCount(
            TokenCountEvent {
                info: Some(TokenUsageInfo {
                    total_token_usage: TokenUsage::default(),
                    last_token_usage: TokenUsage::default(),
                    model_context_window: None,
                }),
                rate_limits: None,
            },
        ))];
        let (turns, replay) = build_turns_with_token_usage_replay(&items);
        assert!(replay.into_snapshot(&turns).is_none());
    }

    fn token_usage_history() -> Vec<RolloutItem> {
        vec![
            RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                client_id: None,
                message: "first turn".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            })),
            RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
                message: "first answer".to_string(),
                phase: None,
            })),
            RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
                info: Some(TokenUsageInfo {
                    total_token_usage: TokenUsage {
                        total_tokens: 150,
                        ..Default::default()
                    },
                    last_token_usage: TokenUsage {
                        total_tokens: 90,
                        ..Default::default()
                    },
                    model_context_window: None,
                }),
                rate_limits: None,
            })),
            RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                client_id: None,
                message: "second turn".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            })),
            RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
                info: None,
                rate_limits: None,
            })),
        ]
    }
}
