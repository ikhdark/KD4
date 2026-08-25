pub(crate) mod code_mode;
pub(crate) mod command_execution;
pub(crate) mod command_output_artifact;
pub(crate) mod context;
pub(crate) mod events;
pub(crate) mod exposure;
pub(crate) mod handlers;
pub(crate) mod hook_names;
pub(crate) mod hosted_spec;
pub(crate) mod known_delta_store;
pub(crate) mod lifecycle;
pub(crate) mod network_approval;
pub(crate) mod orchestrator;
pub(crate) mod parallel;
pub(crate) mod registry;
pub(crate) mod router;
pub(crate) mod runtimes;
pub(crate) mod sandboxing;
pub(crate) mod shell_output_summary;
pub(crate) mod spec_plan;
pub(crate) mod tool_dispatch_trace;

use std::borrow::Cow;

use crate::session::turn_context::TurnContext;
use codex_features::Feature;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::openai_models::ToolMode;
use codex_tools::ToolName;
use codex_utils_output_truncation::OutputLimitResolution;
use codex_utils_output_truncation::OutputOutcome;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::formatted_truncate_text_with_output_limit;
use codex_utils_output_truncation::resolve_output_limits;
use codex_utils_output_truncation::truncate_text;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use codex_utils_output_truncation::truncate_text_with_output_limit;
pub use router::ToolRouter;
use shell_output_summary::ShellOutputSummaryOptions;
use shell_output_summary::summarize_shell_output_for_model;

pub(crate) const SHELL_COMMAND_TOOL_NAME: &str = "shell_command";
pub(crate) const EXEC_COMMAND_TOOL_NAME: &str = "exec_command";

pub(crate) fn is_shell_family_tool_name(tool_name: &ToolName) -> bool {
    tool_name.namespace.is_none()
        && matches!(
            tool_name.name.as_str(),
            SHELL_COMMAND_TOOL_NAME | EXEC_COMMAND_TOOL_NAME
        )
}

/// Legacy boundaries such as hook payloads, telemetry tags, and Responses tool
/// names still require a single flattened string. Keep comparisons and sorting
/// on `ToolName` itself; use this only when crossing those boundaries.
pub(crate) fn flat_tool_name(tool_name: &ToolName) -> Cow<'_, str> {
    match tool_name.namespace.as_deref() {
        Some(namespace) => {
            let mut name = String::with_capacity(namespace.len() + tool_name.name.len());
            name.push_str(namespace);
            name.push_str(&tool_name.name);
            Cow::Owned(name)
        }
        None => Cow::Borrowed(tool_name.name.as_str()),
    }
}

fn effective_tool_mode(turn_context: &TurnContext) -> ToolMode {
    if crate::guardian::is_guardian_reviewer_source(&turn_context.session_source) {
        return ToolMode::Direct;
    }

    turn_context.model_info.tool_mode.unwrap_or_else(|| {
        if turn_context.config.features.enabled(Feature::CodeModeOnly) {
            ToolMode::CodeModeOnly
        } else if turn_context.config.features.enabled(Feature::CodeMode) {
            ToolMode::CodeMode
        } else {
            ToolMode::Direct
        }
    })
}

/// Format the combined exec output for sending back to the model.
/// Includes exit code and duration metadata; truncates large bodies safely.
pub fn format_exec_output_for_model(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
) -> String {
    let token_ceiling = match &truncation_policy {
        TruncationPolicy::Tokens(tokens) => Some(*tokens),
        TruncationPolicy::Bytes(_) => None,
    };
    // round to 1 decimal place
    let duration_seconds = ((exec_output.duration.as_secs_f32()) * 10.0).round() / 10.0;

    let raw_content = build_content_with_timeout(exec_output);
    let content = summarize_shell_output_for_model(
        &raw_content,
        exec_output.exit_code,
        exec_output.timed_out,
        ShellOutputSummaryOptions {
            enabled: true,
            turn_cost_guard: false,
            command_text: None,
        },
    )
    .unwrap_or(raw_content);

    let total_lines = content.lines().count();

    let formatted_output = truncate_text(&content, truncation_policy);

    let mut sections = Vec::new();

    sections.push(format!("Exit code: {}", exec_output.exit_code));
    sections.push(format!("Wall time: {duration_seconds} seconds"));
    if total_lines != formatted_output.lines().count() {
        sections.push(format!("Total output lines: {total_lines}"));
    }

    sections.push("Output:".to_string());
    sections.push(formatted_output);

    let rendered = sections.join("\n");
    token_ceiling
        .map(|max_tokens| truncate_text_to_token_ceiling(&rendered, max_tokens))
        .unwrap_or(rendered)
}

pub(crate) fn project_exec_output_for_model_with_budget(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
    requested_limit: Option<usize>,
    command_text: Option<&str>,
) -> FormattedExecOutput {
    let duration_seconds = ((exec_output.duration.as_secs_f32()) * 10.0).round() / 10.0;
    let raw_content = build_content_with_timeout(exec_output);
    let summarized = summarize_shell_output_for_model(
        &raw_content,
        exec_output.exit_code,
        exec_output.timed_out,
        ShellOutputSummaryOptions {
            enabled: true,
            turn_cost_guard: false,
            command_text,
        },
    );
    let content = summarized.as_deref().unwrap_or(&raw_content);
    let limits = resolve_exec_output_limits(
        exec_output,
        requested_limit,
        command_text,
        &raw_content,
        truncation_policy,
    );
    let truncated = truncate_text_with_output_limit(content, limits);
    let total_lines = content.lines().count();

    let mut sections = vec![
        format!("Exit code: {}", exec_output.exit_code),
        format!("Wall time: {duration_seconds} seconds"),
    ];
    if truncated.was_truncated {
        sections.push(format!("Total output lines: {total_lines}"));
    }
    sections.push("Output:".to_string());
    sections.push(truncated.text);

    let rendered = sections.join("\n");
    FormattedExecOutput {
        text: truncate_text_to_token_ceiling(&rendered, limits.applied_limit),
        reduced: summarized.is_some() || truncated.was_truncated,
    }
}

pub fn format_exec_output_str(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
) -> String {
    project_exec_output_text(exec_output, truncation_policy).text
}

pub(crate) struct FormattedExecOutput {
    pub(crate) text: String,
    pub(crate) reduced: bool,
}

pub(crate) fn project_exec_output_text(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
) -> FormattedExecOutput {
    project_exec_output_text_with_budget(
        exec_output,
        truncation_policy,
        /*requested_limit*/ None,
        /*command_text*/ None,
    )
}

pub(crate) fn project_exec_output_text_with_budget(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
    requested_limit: Option<usize>,
    command_text: Option<&str>,
) -> FormattedExecOutput {
    let raw_content = build_content_with_timeout(exec_output);
    let summarized = summarize_shell_output_for_model(
        &raw_content,
        exec_output.exit_code,
        exec_output.timed_out,
        ShellOutputSummaryOptions {
            enabled: true,
            turn_cost_guard: false,
            command_text,
        },
    );
    let content = summarized.as_deref().unwrap_or(&raw_content);
    let limits = resolve_exec_output_limits(
        exec_output,
        requested_limit,
        command_text,
        &raw_content,
        truncation_policy,
    );
    let truncated = formatted_truncate_text_with_output_limit(content, limits);
    FormattedExecOutput {
        reduced: summarized.is_some() || truncated.was_truncated,
        text: truncated.text,
    }
}

fn resolve_exec_output_limits(
    exec_output: &ExecToolCallOutput,
    requested_limit: Option<usize>,
    command_text: Option<&str>,
    output_text: &str,
    truncation_policy: TruncationPolicy,
) -> OutputLimitResolution {
    resolve_output_limits(
        requested_limit,
        OutputOutcome::from_exit_status(Some(exec_output.exit_code), exec_output.timed_out),
        command_text,
        output_text,
        truncation_policy.token_budget(),
    )
}

/// Extracts exec output content and prepends a timeout message if the command timed out.
fn build_content_with_timeout(exec_output: &ExecToolCallOutput) -> String {
    if exec_output.timed_out {
        format!(
            "command timed out after {} milliseconds\n{}",
            exec_output.duration.as_millis(),
            exec_output.aggregated_output.text
        )
    } else {
        exec_output.aggregated_output.text.clone()
    }
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
