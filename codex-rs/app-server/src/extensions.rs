use std::sync::Arc;

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

