use std::io::Write;
use std::path::Path;

use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
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

    /// Return a delivery failure so the runtime can shut down before returning it.
    fn take_output_error(&mut self) -> Option<std::io::Error> {
        None
    }

    fn print_final_output(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn handle_last_message(
    last_agent_message: Option<&str>,
    output_file: &Path,
) -> std::io::Result<()> {
    let message = last_agent_message.unwrap_or_default();
    write_last_message(output_file, message).map_err(|error| {
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

fn write_last_message(output_file: &Path, message: &str) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(output_file) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    // Follow symlinks and keep device/pipe targets as writable streams.
    if metadata
        .as_ref()
        .is_some_and(|metadata| !metadata.is_file())
    {
        return std::fs::write(output_file, message);
    }
    let parent = output_file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    if let Some(metadata) = metadata {
        if metadata.permissions().readonly() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "last message file is read-only",
            ));
        }
        staged.as_file().set_permissions(metadata.permissions())?;
    }
    staged.write_all(message.as_bytes())?;
    staged.flush()?;
    // std uses POSIX replacement semantics on supported Windows versions,
    // retaining existing readers of the old generation.
    std::fs::rename(staged.path(), output_file)?;
    Ok(())
}

pub(crate) fn final_message_from_turn_items(items: &[ThreadItem]) -> Option<String> {
    items
        .iter()
        .rev()
        .find_map(|item| match item {
            ThreadItem::AgentMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .or_else(|| {
            items.iter().rev().find_map(|item| match item {
                ThreadItem::Plan { text, .. } => Some(text.clone()),
                _ => None,
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_message_replaces_complete_artifact_and_preserves_read_only_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("answer.txt");
        std::fs::write(&path, "old answer").expect("seed artifact");
        let old = std::fs::File::open(&path).expect("open old artifact");
        handle_last_message(Some("new answer"), &path).expect("replace artifact");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read new artifact"),
            "new answer"
        );
        assert_eq!(
            std::io::read_to_string(old).expect("read original handle"),
            "old answer"
        );
        let permissions = std::fs::metadata(&path).expect("metadata").permissions();
        let mut read_only = permissions.clone();
        read_only.set_readonly(true);
        std::fs::set_permissions(&path, read_only).expect("read-only target");
        let result = handle_last_message(Some("must not replace"), &path);
        std::fs::set_permissions(&path, permissions).expect("restore permissions");
        assert_eq!(
            result.expect_err("read-only write must fail").kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read preserved artifact"),
            "new answer"
        );
    }

    #[test]
    fn final_message_prefers_latest_agent_message_and_falls_back_to_plan() {
        let items = vec![
            ThreadItem::Plan {
                id: "plan".to_string(),
                text: "latest plan".to_string(),
            },
            ThreadItem::AgentMessage {
                id: "first".to_string(),
                text: "first answer".to_string(),
                phase: None,
                memory_citation: None,
            },
            ThreadItem::AgentMessage {
                id: "latest".to_string(),
                text: "latest answer".to_string(),
                phase: None,
                memory_citation: None,
            },
        ];
        assert_eq!(
            final_message_from_turn_items(&items),
            Some("latest answer".to_string())
        );
        assert_eq!(
            final_message_from_turn_items(&items[..1]),
            Some("latest plan".to_string())
        );
    }
}
