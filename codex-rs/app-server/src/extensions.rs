use std::sync::Arc;

#[cfg(test)]
use codex_analytics::AnalyticsEventsClient;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadGoal;
use codex_app_server_protocol::ThreadGoalUpdatedNotification;
use codex_app_server_protocol::WarningNotification;
use codex_builtin_extensions::BuiltinExtensionDependencies;
use codex_builtin_extensions::install_builtin_extensions;
use codex_core::config::Config;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::ExtensionRegistryBuilder;
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
    fn emit_for_thread(&self, thread_id: ThreadId, event: Event) {
        match event.msg {
            EventMsg::Warning(warning) => {
                self.outgoing
                    .try_send_server_notification(ServerNotification::Warning(
                        WarningNotification {
                            thread_id: Some(thread_id.to_string()),
                            message: warning.message,
                        },
                    ));
            }
            _ => self.emit(event),
        }
    }

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
    async fn builtin_skill_warning_reaches_client_with_owning_thread() {
        let codex_home = tempfile::TempDir::new().expect("temporary codex home");
        let mut config = core_test_support::load_default_config_for_test(&codex_home).await;
        config.include_skill_instructions = true;
        config.orchestrator_skills_enabled = false;
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            AnalyticsEventsClient::disabled(),
        ));
        let registry = thread_extensions(
            app_server_extension_event_sink(outgoing, ThreadStateManager::new()),
            BuiltinExtensionDependencies {
                auth_manager: codex_login::AuthManager::from_auth_for_testing(
                    codex_login::CodexAuth::from_api_key("test-api-key"),
                ),
                state_db: None,
                analytics_events_client: None,
                thread_manager: std::sync::Weak::new(),
                goal_service: Arc::new(codex_goal_extension::GoalService::new()),
                environment_manager: Arc::new(
                    codex_exec_server::EnvironmentManager::default_for_tests(),
                ),
                session_source: codex_protocol::protocol::SessionSource::Exec,
            },
        );
        let thread_id = ThreadId::new();
        let session_store = codex_extension_api::ExtensionData::new("session");
        let thread_store = codex_extension_api::ExtensionData::new(thread_id.to_string());
        let turn_store = codex_extension_api::ExtensionData::new("turn-warning");
        for contributor in registry.thread_lifecycle_contributors() {
            contributor
                .on_thread_start(codex_extension_api::ThreadStartInput {
                    config: &config,
                    session_source: &codex_protocol::protocol::SessionSource::Exec,
                    persistent_thread_state_available: false,
                    environments: &[],
                    session_store: &session_store,
                    thread_store: &thread_store,
                })
                .await;
        }
        let input = codex_extension_api::TurnInputContext {
            turn_id: "turn-warning".to_string(),
            user_input: Vec::new(),
            environments: Vec::new(),
            ready_selected_capability_roots: vec![
                codex_protocol::capabilities::SelectedCapabilityRoot {
                    id: "missing-skills".to_string(),
                    location: codex_protocol::capabilities::CapabilityRootLocation::Environment {
                        environment_id: "unavailable-executor".to_string(),
                        path: codex_utils_path_uri::PathUri::parse("file:///skills")
                            .expect("skill root URI"),
                    },
                },
            ],
        };
        for contributor in registry.turn_input_contributors() {
            contributor
                .contribute(input.clone(), &session_store, &thread_store, &turn_store)
                .await;
        }
        let notification = timeout(Duration::from_secs(1), outgoing_rx.recv())
            .await
            .expect("registered skill warning must reach the client")
            .expect("outgoing channel remains open");
        let crate::outgoing_message::OutgoingEnvelope::Broadcast {
            message:
                crate::outgoing_message::OutgoingMessage::AppServerNotification(
                    ServerNotification::Warning(warning),
                ),
        } = notification
        else {
            panic!("expected a warning notification");
        };
        assert_eq!(warning.thread_id, Some(thread_id.to_string()));
        assert_eq!(
            warning.message,
            "Selected capability root `missing-skills` references unavailable environment `unavailable-executor`."
        );
        assert!(
            outgoing_rx.try_recv().is_err(),
            "warning must be emitted once"
        );
    }

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
