use std::time::Duration;

use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;

use crate::context::ContextualUserFragment;
use crate::context::UserShellCommand;

fn user_shell_command_fragment(
    command: &str,
    exit_code: i32,
    duration: Duration,
    output: String,
    _truncation_policy: TruncationPolicy,
) -> UserShellCommand {
    UserShellCommand::new(command, exit_code, duration, output)
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
