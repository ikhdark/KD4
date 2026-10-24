use std::sync::Arc;

#[cfg(test)]
use codex_analytics::AnalyticsEventsClient;
use codex_app_server_protocol::ServerNotification;
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
 ) -> Arc<dyn ExtensionEventSink> {
    Arc::new(AppServerExtensionEventSink { outgoing })
}

struct AppServerExtensionEventSink {
    outgoing: Arc<OutgoingMessageSender>,
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
        tracing::debug!(event_id = %event.id, msg = ?event.msg, "dropping unsupported extension event");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
            app_server_extension_event_sink(outgoing),
            BuiltinExtensionDependencies {
                auth_manager: codex_login::AuthManager::from_auth_for_testing(
                    codex_login::CodexAuth::from_api_key("test-api-key"),
                ),
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
        turn_store.insert(codex_core::skills::model::HostSkillsSnapshot::new(
            Arc::new(codex_core::skills::SkillLoadOutcome::default()),
        ));
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
            bound_selected_capability_roots: Vec::new(),
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
                .contribute(&input, &session_store, &thread_store, &turn_store)
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
            "executor skills unavailable: Selected capability root `missing-skills` references unavailable environment `unavailable-executor`."
        );
        assert!(
            outgoing_rx.try_recv().is_err(),
            "warning must be emitted once"
        );
    }


}
