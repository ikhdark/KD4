use std::sync::Arc;

#[cfg(test)]
use codex_analytics::AnalyticsEventsClient;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadGoal;
use codex_app_server_protocol::ThreadGoalUpdatedNotification;
use codex_builtin_extensions::BuiltinExtensionDependencies;
use codex_builtin_extensions::install_builtin_extensions;
use codex_core::config::Config;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
#[cfg(test)]
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;

use crate::outgoing_message::OutgoingMessageSender;
#[cfg(test)]
use crate::thread_state::THREAD_LISTENER_COMMAND_CAPACITY;
use crate::thread_state::ThreadListenerCommand;
use crate::thread_state::ThreadStateManager;
#[cfg(test)]
use crate::thread_state::thread_listener_command_channel;

pub(crate) fn thread_extensions(
    event_sink: Arc<dyn ExtensionEventSink>,
    dependencies: BuiltinExtensionDependencies,
) -> Arc<ExtensionRegistry<Config>> {
    let mut builder = ExtensionRegistryBuilder::<Config>::with_event_sink(event_sink);
    install_builtin_extensions(&mut builder, dependencies);
    Arc::new(builder.build())
}

pub(crate) fn app_server_extension_event_sink(
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
) -> Arc<dyn ExtensionEventSink> {
    Arc::new(AppServerExtensionEventSink {
        outgoing,
        thread_state_manager,
    })
}

struct AppServerExtensionEventSink {
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
}

impl ExtensionEventSink for AppServerExtensionEventSink {
    fn emit(&self, event: Event) {
        match event.msg {
            EventMsg::ThreadGoalUpdated(thread_goal_event) => {
                let thread_id = thread_goal_event.thread_id;
                let turn_id = thread_goal_event.turn_id;
                let goal: ThreadGoal = thread_goal_event.goal.into();
                if let Some(listener_command_tx) = self
                    .thread_state_manager
                    .current_listener_command_tx(thread_id)
                {
                    let command = ThreadListenerCommand::EmitThreadGoalUpdated {
                        turn_id: turn_id.clone(),
                        goal: goal.clone(),
                    };
                    match listener_command_tx.try_send(command) {
                        Ok(()) => return,
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            tracing::warn!(
                                %thread_id,
                                capacity = crate::thread_state::THREAD_LISTENER_COMMAND_CAPACITY,
                                "extension goal update exceeded listener command capacity; sending an explicit unordered fallback notification"
                            );
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            tracing::warn!(
                                "failed to enqueue extension goal update for {thread_id}: listener command channel is closed"
                            );
                        }
                    }
                }
                self.outgoing
                    .try_send_server_notification(ServerNotification::ThreadGoalUpdated(
                        ThreadGoalUpdatedNotification {
                            thread_id: thread_id.to_string(),
                            turn_id,
                            goal,
                        },
                    ));
            }
            msg => {
                tracing::debug!(event_id = %event.id, ?msg, "dropping unsupported extension event");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use codex_protocol::protocol::ThreadGoal as CoreThreadGoal;
    use codex_protocol::protocol::ThreadGoalStatus;
    use codex_protocol::protocol::ThreadGoalUpdatedEvent;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    use super::*;

    #[tokio::test]
    async fn app_server_event_sink_uses_listener_fifo_for_goal_updates_and_clears() {
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            AnalyticsEventsClient::disabled(),
        ));
        let thread_state_manager = ThreadStateManager::new();
        let thread_id = ThreadId::default();
        let (listener_command_tx, mut listener_command_rx) = thread_listener_command_channel();
        thread_state_manager.register_listener_command_tx(thread_id, listener_command_tx.clone());
        let sink = app_server_extension_event_sink(outgoing, thread_state_manager);

        for turn_id in ["turn-1", "turn-2"] {
            sink.emit(thread_goal_updated_event(thread_id, turn_id));
        }
        listener_command_tx
            .send(ThreadListenerCommand::EmitThreadGoalCleared)
            .await
            .expect("listener command channel should be open");

        let mut observed = Vec::new();
        for _ in 0..3 {
            let command = timeout(Duration::from_secs(1), listener_command_rx.recv())
                .await
                .expect("timed out waiting for listener command")
                .expect("listener command channel closed unexpectedly");
            match command {
                ThreadListenerCommand::EmitThreadGoalUpdated { turn_id, .. } => {
                    observed.push(turn_id.expect("extension goal updates should include turn ids"));
                }
                ThreadListenerCommand::EmitThreadGoalCleared => {
                    observed.push("cleared".to_string())
                }
                _ => panic!("unexpected listener command"),
            }
        }

        assert_eq!(
            vec![
                "turn-1".to_string(),
                "turn-2".to_string(),
                "cleared".to_string()
            ],
            observed
        );
    }

    #[tokio::test]
    async fn listener_command_admission_is_bounded_with_observable_overflow() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            AnalyticsEventsClient::disabled(),
        ));
        let thread_state_manager = ThreadStateManager::new();
        let thread_id = ThreadId::default();
        let (listener_command_tx, mut listener_command_rx) = thread_listener_command_channel();
        thread_state_manager.register_listener_command_tx(thread_id, listener_command_tx);
        let sink = app_server_extension_event_sink(outgoing, thread_state_manager);

        const COMMAND_COUNT: usize = THREAD_LISTENER_COMMAND_CAPACITY + 1;
        for index in 0..COMMAND_COUNT {
            sink.emit(thread_goal_updated_event(
                thread_id,
                &format!("turn-{index}"),
            ));
        }

        assert_eq!(listener_command_rx.len(), THREAD_LISTENER_COMMAND_CAPACITY);
        let overflow = outgoing_rx
            .try_recv()
            .expect("overload must surface as an explicit fallback notification");
        let crate::outgoing_message::OutgoingEnvelope::Broadcast { message } = overflow else {
            panic!("expected a broadcast overflow notification");
        };
        let crate::outgoing_message::OutgoingMessage::AppServerNotification(
            ServerNotification::ThreadGoalUpdated(overflow),
        ) = message
        else {
            panic!("expected an overflow goal notification");
        };
        let overflow_turn_id = format!("turn-{THREAD_LISTENER_COMMAND_CAPACITY}");
        assert_eq!(overflow.turn_id.as_deref(), Some(overflow_turn_id.as_str()));
        for index in 0..THREAD_LISTENER_COMMAND_CAPACITY {
            let command = listener_command_rx
                .recv()
                .await
                .expect("admitted listener command");
            let ThreadListenerCommand::EmitThreadGoalUpdated { turn_id, .. } = command else {
                panic!("expected ordered goal update command");
            };
            let expected_turn_id = format!("turn-{index}");
            assert_eq!(turn_id.as_deref(), Some(expected_turn_id.as_str()));
        }
        assert!(listener_command_rx.try_recv().is_err());
    }

    fn thread_goal_updated_event(thread_id: ThreadId, turn_id: &str) -> Event {
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::ThreadGoalUpdated(ThreadGoalUpdatedEvent {
                thread_id,
                turn_id: Some(turn_id.to_string()),
                goal: CoreThreadGoal {
                    thread_id,
                    objective: "wire extension events".to_string(),
                    status: ThreadGoalStatus::Active,
                    token_budget: Some(123),
                    tokens_used: 45,
                    time_used_seconds: 6,
                    created_at: 7,
                    updated_at: 8,
                },
            }),
        }
    }
}
