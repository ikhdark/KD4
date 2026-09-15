//! Shared builders for app-server [`ThreadItem`] values derived from compatibility events.
//!
//! Most live tool items now come from first-class core `ItemStarted` / `ItemCompleted` events.
//! These builders remain for approval flows, rebuilt legacy history, and other pre-execution
//! paths where the underlying tool has not started or never starts at all.
//!
//! Keeping these builders in one place is useful for two reasons:
//! - Live notifications and rebuilt `thread/read` history both need to construct the same
//!   synthetic items, so sharing the logic avoids drift between those paths.
//! - The projection is presentation-specific. Core protocol events stay generic, while the
//!   app-server protocol decides how to surface those events as `ThreadItem`s for clients.
use crate::protocol::v2::CommandAction;
use crate::protocol::v2::CommandExecutionStatus;
use crate::protocol::v2::FileUpdateChange;
use crate::protocol::v2::PatchApplyStatus;
use crate::protocol::v2::PatchChangeKind;
use crate::protocol::v2::ThreadItem;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::ExecCommandBeginEvent;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyBeginEvent;
use codex_protocol::protocol::PatchApplyEndEvent;
use codex_protocol::protocol::ReviewOutputEvent;
use codex_protocol::review_format::REVIEW_FALLBACK_MESSAGE;
use codex_protocol::review_format::render_review_output_text;
use codex_shell_command::parse_command::shlex_join;
use codex_utils_absolute_path::is_windows_absolute_path;
use codex_utils_path_uri::PathConvention;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::warn;

pub(crate) fn review_output_text(output: Option<&ReviewOutputEvent>) -> String {
    output
        .map(render_review_output_text)
        .unwrap_or_else(|| REVIEW_FALLBACK_MESSAGE.to_string())
}

pub fn build_file_change_approval_request_item(
    payload: &ApplyPatchApprovalRequestEvent,
) -> ThreadItem {
    ThreadItem::FileChange {
        id: payload.call_id.clone(),
        changes: convert_patch_changes(&payload.changes),
        status: PatchApplyStatus::InProgress,
    }
}

pub fn build_file_change_begin_item(payload: &PatchApplyBeginEvent) -> ThreadItem {
    ThreadItem::FileChange {
        id: payload.call_id.clone(),
        changes: convert_patch_changes(&payload.changes),
        status: PatchApplyStatus::InProgress,
    }
}

pub fn build_file_change_end_item(payload: &PatchApplyEndEvent) -> ThreadItem {
    ThreadItem::FileChange {
        id: payload.call_id.clone(),
        changes: convert_patch_changes(&payload.changes),
        status: (&payload.status).into(),
    }
}

pub fn build_command_execution_begin_item(payload: &ExecCommandBeginEvent) -> ThreadItem {
    let command_actions = command_actions_for_path_uri(&payload.parsed_cmd, &payload.cwd);
    ThreadItem::CommandExecution {
        id: payload.call_id.clone(),
        command: command_display_string(&payload.command),
        cwd: payload.cwd.clone().into(),
        process_id: payload.process_id.clone(),
        parent_call_id: None,
        parent_cell_id: None,
        runtime_tool_call_id: None,
        execution_id: None,
        source: payload.source.into(),
        status: CommandExecutionStatus::InProgress,
        command_actions,
        aggregated_output: None,
        exit_code: None,
        duration_ms: None,
    }
}

pub fn build_command_execution_end_item(payload: &ExecCommandEndEvent) -> ThreadItem {
    let aggregated_output = if payload.aggregated_output.is_empty() {
        None
    } else {
        Some(payload.aggregated_output.clone())
    };
    let duration_ms = i64::try_from(payload.duration.as_millis()).unwrap_or(i64::MAX);
    let command_actions = command_actions_for_path_uri(&payload.parsed_cmd, &payload.cwd);

    ThreadItem::CommandExecution {
        id: payload.call_id.clone(),
        command: command_display_string(&payload.command),
        cwd: payload.cwd.clone().into(),
        process_id: payload.process_id.clone(),
        parent_call_id: None,
        parent_cell_id: None,
        runtime_tool_call_id: None,
        execution_id: None,
        source: payload.source.into(),
        status: (&payload.status).into(),
        command_actions,
        aggregated_output,
        exit_code: Some(payload.exit_code),
        duration_ms: Some(duration_ms),
    }
}

/// Format argv for app-server command presentation without changing execution semantics.
///
/// Commands whose executable is an absolute Windows path use Windows argv quoting so
/// backslashes remain readable and arguments still round-trip through the CRT parsing rules.
/// Other commands keep the existing POSIX-style display, including its NUL-byte placeholder.
pub fn command_display_string(command: &[String]) -> String {
    if command.iter().any(|arg| arg.contains('\0')) {
        return shlex_join(command);
    }

    if command
        .first()
        .is_some_and(|program| is_windows_absolute_path(program))
    {
        command
            .iter()
            .map(|arg| codex_shell_command::quote_windows_arg(arg))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        shlex_join(command)
    }
}

pub(crate) fn command_actions_for_path_uri(
    parsed_cmd: &[ParsedCommand],
    cwd: &PathUri,
) -> Vec<CommandAction> {
    // TODO(anp): Carry PathUri into CommandAction so foreign Read actions retain resolved paths.
    // Until then, omit those actions rather than project a foreign cwd onto the host.
    let native_cwd = if cwd.infer_path_convention() == Some(PathConvention::native()) {
        match cwd.to_abs_path() {
            Ok(path) => Some(path),
            Err(error) => {
                warn!(%cwd, %error, "cannot resolve native command cwd");
                None
            }
        }
    } else {
        None
    };

    parsed_cmd
        .iter()
        .cloned()
        .filter_map(|parsed| match parsed {
            ParsedCommand::Read { cmd, name, path } => {
                native_cwd.as_ref().map(|native_cwd| CommandAction::Read {
                    command: cmd,
                    name,
                    path: native_cwd.join(path),
                })
            }
            ParsedCommand::ListFiles { cmd, path } => {
                Some(CommandAction::ListFiles { command: cmd, path })
            }
            ParsedCommand::Search { cmd, query, path } => Some(CommandAction::Search {
                command: cmd,
                query,
                path,
            }),
            ParsedCommand::Unknown { cmd } => Some(CommandAction::Unknown { command: cmd }),
        })
        .collect()
}

pub fn convert_patch_changes(changes: &HashMap<PathBuf, FileChange>) -> Vec<FileUpdateChange> {
    let mut converted: Vec<FileUpdateChange> = changes
        .iter()
        .map(|(path, change)| FileUpdateChange {
            path: path.to_string_lossy().into_owned(),
            kind: map_patch_change_kind(change),
            diff: format_file_change_diff(change),
        })
        .collect();
    converted.sort_by(|a, b| a.path.cmp(&b.path));
    converted
}

fn map_patch_change_kind(change: &FileChange) -> PatchChangeKind {
    match change {
        FileChange::Add { .. } => PatchChangeKind::Add,
        FileChange::Delete { .. } => PatchChangeKind::Delete,
        FileChange::Update { move_path, .. } => PatchChangeKind::Update {
            move_path: move_path.clone(),
        },
    }
}

fn format_file_change_diff(change: &FileChange) -> String {
    match change {
        FileChange::Add { content } => content.clone(),
        FileChange::Delete { content } => content.clone(),
        FileChange::Update {
            unified_diff,
            move_path,
        } => {
            if let Some(path) = move_path {
                format!("{unified_diff}\n\nMoved to: {}", path.display())
            } else {
                unified_diff.clone()
            }
        }
    }
}

#[cfg(test)]
#[path = "item_builders_tests.rs"]
mod tests;
