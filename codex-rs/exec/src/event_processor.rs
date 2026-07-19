use std::path::Path;

use codex_app_server_protocol::ServerNotification;
use codex_core::config::Config;
use codex_protocol::protocol::SessionConfiguredEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexStatus {
    Running,
    InitiateShutdown,
}

pub(crate) trait EventProcessor {
    /// Print summary of effective configuration and user prompt.
    fn print_config_summary(
        &mut self,
        config: &Config,
        prompt: &str,
        session_configured: &SessionConfiguredEvent,
    );

    /// Handle a single typed app-server notification emitted by the agent.
    fn process_server_notification(&mut self, notification: ServerNotification) -> CodexStatus;

    /// Handle a local exec warning that is not represented as an app-server notification.
    fn process_warning(&mut self, message: String) -> CodexStatus;

    /// Handle an unrecoverable failure in exec's local app-server event stream.
    fn process_event_stream_error(&mut self, message: String);

    fn print_final_output(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn handle_last_message(
    last_agent_message: Option<&str>,
    output_file: &Path,
) -> std::io::Result<()> {
    let message = last_agent_message.unwrap_or_default();
    std::fs::write(output_file, message).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "failed to write last message file {}: {error}",
                output_file.display()
            ),
        )
    })?;
    if last_agent_message.is_none() {
        eprintln!(
            "Warning: no last agent message; wrote empty content to {}",
            output_file.display()
        );
    }
    Ok(())
}
