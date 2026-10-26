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
    /// Host observations only: neither hook acknowledges provider acceptance or client rendering.
    fn record_turn_submission(&mut self) {}

    fn record_server_notification_received(&mut self) {}

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

    /// Invocation failure after core completion; never invent a second terminal
    /// or revoke an already accepted final artifact.
    fn process_invocation_error(&mut self, message: String) {
        eprintln!("Session finalization failed: {message}");
    }

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

/// Bounded streaming counterpart of the recovered-item selector. Keep only the
/// best eligible message; commentary remains in the transcript, not this cache.
#[derive(Default)]
pub(crate) struct FinalMessageSelection {
    message: Option<String>,
    rank: u8,
}

impl FinalMessageSelection {
    pub(crate) fn observe(&mut self, item: &ThreadItem) {
        let Some((text, rank)) = final_message_candidate(item) else {
            return;
        };
        if rank >= self.rank {
            self.message = Some(text.to_string());
            self.rank = rank;
        }
    }

    pub(crate) fn message(&self) -> Option<String> {
        self.message.clone()
    }
}

pub(crate) fn final_message_from_turn_items(items: &[ThreadItem]) -> Option<String> {
    items
        .iter()
        .filter_map(final_message_candidate)
        .max_by_key(|(_, rank)| *rank)
        .map(|(text, _)| text.to_string())
}

fn final_message_candidate(item: &ThreadItem) -> Option<(&str, u8)> {
    use codex_protocol::models::MessagePhase;
    match item {
        ThreadItem::AgentMessage {
            text,
            phase: Some(MessagePhase::FinalAnswer),
            ..
        } => Some((text, 3)),
        ThreadItem::AgentMessage {
            text, phase: None, ..
        } => Some((text, 2)),
        ThreadItem::Plan { text, .. } => Some((text, 1)),
        _ => None,
    }
}

pub(crate) fn final_message_from_turn(
    items: &[ThreadItem],
    timing: Option<&codex_protocol::protocol::TurnTiming>,
) -> Option<String> {
    if let Some(answer) = timing.and_then(|timing| timing.final_answer.as_ref()) {
        return answer.message.as_ref().map(|message| message.text.clone());
    }
    // Legacy producers carry no acceptance receipt. Never promote commentary.
    final_message_from_turn_items(items)
}

/// Some(None) is an authoritative no-final receipt, not missing evidence.
pub(crate) fn authoritative_final_message(
    notification: &codex_app_server_protocol::TurnCompletedNotification,
) -> Option<Option<&str>> {
    if let Some(result) = notification
        .surfaced_result
        .as_ref()
        .or(notification.turn.surfaced_result.as_ref())
    {
        return Some(result.canonical_message.as_deref());
    }
    notification
        .timing
        .as_ref()
        .or(notification.turn.timing.as_ref())
        .and_then(|timing| timing.final_answer.as_ref())
        .map(|answer| answer.message.as_ref().map(|message| message.text.as_str()))
}

pub(crate) fn item_is_in_progress(item: &ThreadItem) -> bool {
    use codex_app_server_protocol::CollabAgentToolCallStatus;
    use codex_app_server_protocol::CommandExecutionStatus;
    use codex_app_server_protocol::DynamicToolCallStatus;
    use codex_app_server_protocol::McpToolCallStatus;
    use codex_app_server_protocol::PatchApplyStatus;

    match item {
        ThreadItem::CommandExecution { status, .. } => {
            *status == CommandExecutionStatus::InProgress
        }
        ThreadItem::FileChange { status, .. } => *status == PatchApplyStatus::InProgress,
        ThreadItem::McpToolCall { status, .. } => *status == McpToolCallStatus::InProgress,
        ThreadItem::CollabAgentToolCall { status, .. } => {
            *status == CollabAgentToolCallStatus::InProgress
        }
        ThreadItem::DynamicToolCall {
            namespace,
            tool,
            arguments,
            status,
            ..
        } => {
            if namespace.as_deref() == Some("codex.internal") && tool == "code_mode_cell" {
                match arguments.get("state").and_then(|value| value.as_str()) {
                    Some("yielded") => return true,
                    Some("terminated") => return false,
                    _ => {}
                }
            }
            *status == DynamicToolCallStatus::InProgress
        }
        _ => false,
    }
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
        let mut items = vec![
            ThreadItem::Plan {
                id: "plan".to_string(),
                text: "latest plan".to_string(),
            },
            ThreadItem::AgentMessage {
                id: "first".to_string(),
                text: "first answer".to_string(),
                phase: None,
            },
            ThreadItem::AgentMessage {
                id: "latest".to_string(),
                text: "latest answer".to_string(),
                phase: None,
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
        items.push(ThreadItem::Plan {
            id: "newer-plan".to_string(),
            text: "newer plan".to_string(),
        });
        assert_eq!(
            final_message_from_turn_items(&items),
            Some("latest answer".to_string())
        );
        assert_eq!(
            final_message_from_turn_items(&[items[0].clone(), items[3].clone()]),
            Some("newer plan".to_string())
        );
        assert_eq!(final_message_from_turn_items(&[]), None);
    }
}
#[cfg(test)]
mod phase_selection_tests {
    use super::*;
    use codex_protocol::models::MessagePhase;

    #[test]
    fn streamed_and_recovered_selectors_exclude_commentary_and_keep_empty_final() {
        for answer in ["accepted answer", ""] {
            let mut items = vec![
                ThreadItem::AgentMessage {
                    id: "final".into(),
                    text: answer.into(),
                    phase: Some(MessagePhase::FinalAnswer),
                },
                ThreadItem::AgentMessage {
                    id: "progress".into(),
                    text: "I will check next".into(),
                    phase: Some(MessagePhase::Commentary),
                },
                ThreadItem::AgentMessage {
                    id: "legacy".into(),
                    text: "legacy tail".into(),
                    phase: None,
                },
            ];
            let mut selection = FinalMessageSelection::default();
            for item in &items {
                selection.observe(item);
            }
            assert_eq!(selection.message().as_deref(), Some(answer));
            assert_eq!(
                final_message_from_turn_items(&items).as_deref(),
                Some(answer)
            );
            items.remove(0);
            assert_eq!(
                final_message_from_turn_items(&items).as_deref(),
                Some("legacy tail")
            );
            assert_eq!(final_message_from_turn_items(&items[..1]), None);
        }
    }
}
