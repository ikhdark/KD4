use std::sync::Arc;

use codex_app_server_protocol_types::ServerNotification;
use codex_app_server_protocol_types::WarningNotification;
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
    thread_state_manager: crate::thread_state::ThreadStateManager,
) -> Arc<dyn ExtensionEventSink> {
    // Extension callbacks are synchronous. One bounded relay performs the
    // asynchronous subscriber lookup; never spawn one task per warning.
    let (warnings, mut pending) = tokio::sync::mpsc::channel::<(ThreadId, ServerNotification)>(32);
    let outgoing = Arc::downgrade(&outgoing);
    tokio::spawn(async move {
        while let Some((thread_id, notification)) = pending.recv().await {
            let Some(outgoing) = outgoing.upgrade() else { break; };
            let recipients = thread_state_manager.subscribed_connection_ids(thread_id).await;
            // Empty recipient lists mean broadcast to the generic sender.
            if recipients.is_empty() { continue; }
            outgoing.send_server_notification_to_connections(&recipients, notification).await;
        }
    });
    Arc::new(AppServerExtensionEventSink { warnings })
}

struct AppServerExtensionEventSink {
    warnings: tokio::sync::mpsc::Sender<(ThreadId, ServerNotification)>,
}

impl ExtensionEventSink for AppServerExtensionEventSink {
    fn emit_for_thread(&self, thread_id: ThreadId, event: Event) {
        match event.msg {
            EventMsg::Warning(warning) => {
                let identity = codex_protocol::timing::client_probe::enabled().then(|| (
                    codex_protocol::timing::client_probe::key(&thread_id.to_string()),
                    codex_protocol::timing::client_probe::key(&warning.message),
                ));
                let accepted = self.warnings.try_send((thread_id, ServerNotification::Warning(
                    WarningNotification {
                        thread_id: Some(thread_id.to_string()),
                        message: warning.message,
                    },
                ))).is_ok();
                if let Some((thread, message)) = identity {
                    codex_protocol::timing::client_probe::record(120, 0, thread, 0, message,
                        [u64::from(accepted), 0, 0, 0]);
                }
            }
            _ => self.emit(event),
        }
    }

    fn emit(&self, event: Event) {
        tracing::debug!(event_id = %event.id, msg = ?event.msg, "dropping unsupported extension event");
    }
}

