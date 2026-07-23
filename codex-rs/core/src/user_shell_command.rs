use std::time::Duration;

#[cfg(test)]
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::formatted_truncate_text;

use crate::context::ContextualUserFragment;
use crate::context::UserShellCommand;
#[cfg(test)]
use crate::session::turn_context::TurnContext;
#[cfg(test)]
use crate::tools::format_exec_output_str;

fn user_shell_command_fragment(
    command: &str,
    exit_code: i32,
    duration: Duration,
    output: String,
    truncation_policy: TruncationPolicy,
) -> UserShellCommand {
    let command = escape_xml_text(command);
    let output = escape_xml_text(&output);
    let output = formatted_truncate_text(&output, truncation_policy);
    UserShellCommand::new(command, exit_code, duration, output)
}

fn escape_xml_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
fn user_shell_command_fragment_from_exec_output(
    command: &str,
    exec_output: &ExecToolCallOutput,
    turn_context: &TurnContext,
) -> UserShellCommand {
    let truncation_policy = turn_context.model_info.truncation_policy.into();
    let output = format_exec_output_str(exec_output, truncation_policy);
    user_shell_command_fragment(
        command,
        exec_output.exit_code,
        exec_output.duration,
        output,
        truncation_policy,
    )
}

#[cfg(test)]
pub fn format_user_shell_command_record(
    command: &str,
    exec_output: &ExecToolCallOutput,
    turn_context: &TurnContext,
) -> String {
    user_shell_command_fragment_from_exec_output(command, exec_output, turn_context).render()
}

#[cfg(test)]
pub fn user_shell_command_record_item(
    command: &str,
    exec_output: &ExecToolCallOutput,
    turn_context: &TurnContext,
) -> ResponseItem {
    ContextualUserFragment::into(user_shell_command_fragment_from_exec_output(
        command,
        exec_output,
        turn_context,
    ))
}

pub fn user_shell_command_record_item_from_formatted_output(
    command: &str,
    exit_code: i32,
    duration: Duration,
    output: String,
    truncation_policy: TruncationPolicy,
) -> ResponseItem {
    ContextualUserFragment::into(user_shell_command_fragment(
        command,
        exit_code,
        duration,
        output,
        truncation_policy,
    ))
}

#[cfg(test)]
#[path = "user_shell_command_tests.rs"]
mod tests;
