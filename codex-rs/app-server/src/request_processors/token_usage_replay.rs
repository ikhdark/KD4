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
            if matches!(item, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_)))
                && token_usage_replay.turn_owner.as_ref().is_some_and(|owner| {
                    owner.position.is_some_and(|position| {
                        builder.active_turn_position().is_none_or(|last| position > last)
                    })
                })
            {
                // A removed turn's position may be reused by a later turn.
                // Invalidate before that happens rather than relabeling its usage.
                token_usage_replay = TokenUsageReplay::default();
            }
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
    fn replay_attribution_preserves_usage_across_rebuilt_turns() {
        for rebuilt in [false, true] {
            let rollout_items = token_usage_history();
            let (mut turns, replay) = build_turns_with_token_usage_replay(&rollout_items);
            if rebuilt {
                turns[0].id = "rebuilt-turn-id".to_string();
            }
            let snapshot = replay.into_snapshot(&turns).expect("usage snapshot");
            assert_eq!(snapshot.turn_id, turns[0].id);
            assert_eq!(
                snapshot.info,
                TokenUsageInfo {
                    total_token_usage: TokenUsage {
                        input_tokens: 120,
                        cached_input_tokens: 20,
                        output_tokens: 30,
                        reasoning_output_tokens: 10,
                        total_tokens: 150,
                    },
                    last_token_usage: TokenUsage {
                        input_tokens: 70,
                        cached_input_tokens: 10,
                        output_tokens: 20,
                        reasoning_output_tokens: 5,
                        total_tokens: 90,
                    },
                    model_context_window: Some(200_000),
                }
            );
        }
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

    #[test]
    fn rollback_does_not_reassign_usage_to_a_replacement_turn() {
        let mut items = token_usage_history();
        // A rollback marker is persisted separately from any recomputed count.
        // The replacement turn must not inherit usage owned by the removed turn.
        items.insert(
            3,
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
                codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
            )),
        );
        let (turns, replay) = build_turns_with_token_usage_replay(&items);
        assert_eq!(turns.len(), 1);
        assert!(replay.into_snapshot(&turns).is_none());
    }

    #[test]
    fn rollback_preserves_retained_usage_and_accepts_a_fresh_replacement_count() {
        let rollback = RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        ));
        let mut retained = token_usage_history();
        retained.push(rollback.clone());
        let (turns, replay) = build_turns_with_token_usage_replay(&retained);
        assert_eq!(turns.len(), 1);
        assert_eq!(replay.into_snapshot(&turns).expect("A remains").turn_id, turns[0].id);

        let mut replaced = token_usage_history();
        replaced.insert(3, rollback);
        let fresh = replaced[2].clone();
        replaced.push(fresh);
        let (turns, replay) = build_turns_with_token_usage_replay(&replaced);
        assert_eq!(turns.len(), 1);
        assert_eq!(replay.into_snapshot(&turns).expect("fresh B count").turn_id, turns[0].id);
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
                        input_tokens: 120,
                        cached_input_tokens: 20,
                        output_tokens: 30,
                        reasoning_output_tokens: 10,
                        total_tokens: 150,
                    },
                    last_token_usage: TokenUsage {
                        input_tokens: 70,
                        cached_input_tokens: 10,
                        output_tokens: 20,
                        reasoning_output_tokens: 5,
                        total_tokens: 90,
                    },
                    model_context_window: Some(200_000),
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
