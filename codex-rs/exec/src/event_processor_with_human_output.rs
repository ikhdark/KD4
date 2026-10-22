use std::collections::VecDeque;
use std::io::IsTerminal;
use std::path::PathBuf;

use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::McpToolCallStatus;
use codex_app_server_protocol::PatchApplyStatus;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadTokenUsage;
use codex_app_server_protocol::TurnStatus;
use codex_core::config::Config;
use codex_model_provider_info::WireApi;
use codex_protocol::protocol::FinalOutput;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_utils_sandbox_summary::summarize_permission_profile;
use owo_colors::OwoColorize;
use owo_colors::Style;

use crate::event_processor::CodexStatus;
use crate::event_processor::EventProcessor;
use crate::event_processor::final_message_from_turn;
use crate::event_processor::handle_last_message;

pub(crate) struct EventProcessorWithHumanOutput {
    bold: Style,
    cyan: Style,
    dimmed: Style,
    green: Style,
    italic: Style,
    magenta: Style,
    red: Style,
    yellow: Style,
    show_agent_reasoning: bool,
    show_raw_agent_reasoning: bool,
    last_message_path: Option<PathBuf>,
    final_message: Option<String>,
    streamed_final_message: crate::event_processor::FinalMessageSelection,
    final_message_rendered: bool,
    emit_final_message_on_shutdown: bool,
    last_total_token_usage: Option<ThreadTokenUsage>,
    displayed_warnings: VecDeque<String>,
    last_non_retry_error: Option<(String, String)>,
}

impl EventProcessorWithHumanOutput {
    pub(crate) fn create_with_ansi(
        with_ansi: bool,
        config: &Config,
        last_message_path: Option<PathBuf>,
    ) -> Self {
        let style = |styled: Style, plain: Style| if with_ansi { styled } else { plain };
        Self {
            bold: style(Style::new().bold(), Style::new()),
            cyan: style(Style::new().cyan(), Style::new()),
            dimmed: style(Style::new().dimmed(), Style::new()),
            green: style(Style::new().green(), Style::new()),
            italic: style(Style::new().italic(), Style::new()),
            magenta: style(Style::new().magenta(), Style::new()),
            red: style(Style::new().red(), Style::new()),
            yellow: style(Style::new().yellow(), Style::new()),
            show_agent_reasoning: !config.hide_agent_reasoning,
            show_raw_agent_reasoning: config.show_raw_agent_reasoning,
            last_message_path,
            final_message: None,
            streamed_final_message: Default::default(),
            final_message_rendered: false,
            emit_final_message_on_shutdown: false,
            last_total_token_usage: None,
            displayed_warnings: VecDeque::new(),
            last_non_retry_error: None,
        }
    }

    fn render_item_started(&self, item: &ThreadItem) {
        match item {
            ThreadItem::CommandExecution { command, cwd, .. } => {
                eprintln!(
                    "{}\n{} in {cwd}",
                    "exec".style(self.italic).style(self.magenta),
                    command.style(self.bold),
                );
            }
            ThreadItem::McpToolCall { server, tool, .. } => {
                eprintln!(
                    "{} {} {}",
                    "mcp:".style(self.bold),
                    format!("{server}/{tool}").style(self.cyan),
                    "started".style(self.dimmed)
                );
            }
            ThreadItem::WebSearch(item) => {
                eprintln!("{} {}", "web search:".style(self.bold), item.query);
            }
            ThreadItem::FileChange { .. } => {
                eprintln!("{}", "apply patch".style(self.bold));
            }
            ThreadItem::CollabAgentToolCall { tool, .. } => {
                eprintln!("{} {:?}", "collab:".style(self.bold), tool);
            }
            _ => {}
        }
    }

    fn render_item_completed(&mut self, item: ThreadItem) {
        self.streamed_final_message.observe(&item);
        match item {
            ThreadItem::AgentMessage { text, .. } => {
                eprintln!(
                    "{}\n{}",
                    "codex".style(self.italic).style(self.magenta),
                    text
                );
                self.final_message = self.streamed_final_message.message();
                self.final_message_rendered = self.final_message.as_ref() == Some(&text);
            }
            ThreadItem::Reasoning {
                summary, content, ..
            } => {
                if self.show_agent_reasoning
                    && let Some(text) =
                        reasoning_text(&summary, &content, self.show_raw_agent_reasoning)
                    && !text.trim().is_empty()
                {
                    eprintln!("{}", text.style(self.dimmed));
                }
            }
            ThreadItem::CommandExecution {
                command: _,
                aggregated_output,
                exit_code,
                status,
                duration_ms,
                ..
            } => {
                let duration_suffix = duration_ms
                    .map(|duration_ms| format!(" in {duration_ms}ms"))
                    .unwrap_or_default();
                match status {
                    CommandExecutionStatus::Completed => {
                        eprintln!(
                            "{}",
                            format!(" succeeded{duration_suffix}:").style(self.green)
                        );
                    }
                    CommandExecutionStatus::Failed => {
                        let exit_code = exit_code.unwrap_or(1);
                        eprintln!(
                            "{}",
                            format!(" exited {exit_code}{duration_suffix}:").style(self.red)
                        );
                    }
                    CommandExecutionStatus::Declined => {
                        eprintln!(
                            "{}",
                            format!(" declined{duration_suffix}:").style(self.yellow)
                        );
                    }
                    CommandExecutionStatus::InProgress => {
                        eprintln!(
                            "{}",
                            format!(" in progress{duration_suffix}:").style(self.dimmed)
                        );
                    }
                }
                if let Some(output) = aggregated_output
                    && !output.trim().is_empty()
                {
                    eprintln!("{output}");
                }
            }
            ThreadItem::FileChange {
                changes, status, ..
            } => {
                let status_text = match status {
                    PatchApplyStatus::Completed => "completed",
                    PatchApplyStatus::Failed => "failed",
                    PatchApplyStatus::Declined => "declined",
                    PatchApplyStatus::InProgress => "in_progress",
                };
                eprintln!("{} {}", "patch:".style(self.bold), status_text);
                for change in changes {
                    eprintln!("{}", change.path.style(self.dimmed));
                }
            }
            ThreadItem::McpToolCall {
                server,
                tool,
                status,
                error,
                ..
            } => {
                let status_text = match status {
                    McpToolCallStatus::Completed => "completed".style(self.green),
                    McpToolCallStatus::Failed => "failed".style(self.red),
                    McpToolCallStatus::InProgress => "in_progress".style(self.dimmed),
                };
                eprintln!(
                    "{} {} {}",
                    "mcp:".style(self.bold),
                    format!("{server}/{tool}").style(self.cyan),
                    format!("({status_text})").style(self.dimmed)
                );
                if let Some(error) = error {
                    eprintln!("{}", error.message.style(self.red));
                }
            }
            ThreadItem::WebSearch(item) => {
                eprintln!(
                    "{} {} ({})",
                    "web search:".style(self.bold),
                    item.query,
                    item.status.as_deref().unwrap_or("outcome unrecorded")
                );
                if let Some(error) = item.error {
                    eprintln!("{error}");
                }
            }
            ThreadItem::ContextCompaction { .. } => {
                eprintln!("{}", "context compacted".style(self.dimmed));
            }
            _ => {}
        }
    }
}

impl EventProcessor for EventProcessorWithHumanOutput {
    fn print_config_summary(
        &mut self,
        config: &Config,
        prompt: &str,
        session_configured_event: &SessionConfiguredEvent,
    ) {
        const VERSION: &str = codex_utils_build_info::CODEX_VERSION;
        eprintln!("OpenAI Codex v{VERSION}\n--------");
        for (key, value) in config_summary_entries(config, session_configured_event) {
            eprintln!("{} {}", format!("{key}:").style(self.bold), value);
        }
        eprintln!("--------");
        eprintln!("{}\n{}", "user".style(self.cyan), prompt);
    }

    fn process_server_notification(&mut self, notification: ServerNotification) -> CodexStatus {
        if let ServerNotification::TurnCompleted(notification) = &notification
            && let Some(assessment) = notification
                .timing
                .as_ref()
                .or(notification.turn.timing.as_ref())
                .and_then(|timing| timing.completion_assessment.as_ref())
        {
            // Optional warnings can be dropped; terminal metadata cannot. Keep
            // diagnostics on stderr, never in canonical stdout or answer files.
            if let Some(message) = assessment.verification_warning()
                && !self.displayed_warnings.contains(&message)
            {
                // Persistence diagnostics may already have been emitted as
                // individual warnings rather than the combined assessment.
                let mut missing = assessment.clone();
                missing
                    .failed_checks
                    .retain(|check| !self.displayed_warnings.contains(check));
                missing
                    .verification_gaps
                    .retain(|gap| !self.displayed_warnings.contains(gap));
                if let Some(message) = missing.verification_warning() {
                    self.process_warning(message);
                }
            }
        }
        match notification {
            ServerNotification::ConfigWarning(notification) => {
                let details = notification
                    .details
                    .map(|details| format!(" ({details})"))
                    .unwrap_or_default();
                eprintln!(
                    "{} {}{}",
                    "warning:".style(self.yellow).style(self.bold),
                    notification.summary,
                    details
                );
                CodexStatus::Running
            }
            ServerNotification::Warning(notification) => self.process_warning(notification.message),
            ServerNotification::Error(notification) => {
                if !notification.will_retry && !notification.error.message.trim().is_empty() {
                    self.last_non_retry_error = Some((
                        notification.turn_id.clone(),
                        notification.error.message.clone(),
                    ));
                }
                eprintln!(
                    "{} {}",
                    "ERROR:".style(self.red).style(self.bold),
                    notification.error
                );
                CodexStatus::Running
            }
            ServerNotification::DeprecationNotice(notification) => {
                eprintln!(
                    "{} {}",
                    "deprecated:".style(self.yellow).style(self.bold),
                    notification.summary
                );
                if let Some(details) = notification.details {
                    eprintln!("{}", details.style(self.dimmed));
                }
                CodexStatus::Running
            }
            ServerNotification::HookStarted(notification) => {
                eprintln!(
                    "{} {}",
                    "hook:".style(self.bold),
                    format!("{:?}", notification.run.event_name).style(self.dimmed)
                );
                CodexStatus::Running
            }
            ServerNotification::HookCompleted(notification) => {
                eprintln!(
                    "{} {} {:?}",
                    "hook:".style(self.bold),
                    format!("{:?}", notification.run.event_name).style(self.dimmed),
                    notification.run.status
                );
                CodexStatus::Running
            }
            ServerNotification::ItemStarted(notification) => {
                self.render_item_started(&notification.item);
                CodexStatus::Running
            }
            ServerNotification::ItemCompleted(notification) => {
                self.render_item_completed(notification.item);
                CodexStatus::Running
            }
            ServerNotification::ModelRerouted(notification) => {
                eprintln!(
                    "{} {} -> {}",
                    "model rerouted:".style(self.yellow).style(self.bold),
                    notification.from_model,
                    notification.to_model
                );
                CodexStatus::Running
            }
            ServerNotification::ModelVerification(_) => CodexStatus::Running,
            ServerNotification::ThreadTokenUsageUpdated(notification) => {
                self.last_total_token_usage = Some(notification.token_usage);
                CodexStatus::Running
            }
            ServerNotification::TurnCompleted(notification) => match notification.turn.status {
                TurnStatus::Completed => {
                    if let Some(surfaced_result) = notification
                        .surfaced_result
                        .as_ref()
                        .or(notification.turn.surfaced_result.as_ref())
                    {
                        self.final_message_rendered = self.final_message_rendered
                            && self.final_message == surfaced_result.canonical_message;
                        self.final_message = surfaced_result.canonical_message.clone();
                    } else {
                        let timing = notification
                            .timing
                            .as_ref()
                            .or(notification.turn.timing.as_ref());
                        for item in &notification.turn.items {
                            self.streamed_final_message.observe(item);
                        }
                        let final_message =
                            if timing.is_some_and(|timing| timing.final_answer.is_some()) {
                                final_message_from_turn(notification.turn.items.as_slice(), timing)
                            } else {
                                self.streamed_final_message
                                    .message()
                                    .or_else(|| self.final_message.clone())
                            };
                        self.final_message_rendered =
                            self.final_message_rendered && self.final_message == final_message;
                        self.final_message = final_message;
                    }
                    self.emit_final_message_on_shutdown = true;
                    CodexStatus::InitiateShutdown
                }
                TurnStatus::Failed => {
                    self.final_message = None;
                    self.final_message_rendered = false;
                    self.emit_final_message_on_shutdown = false;
                    let detailed = notification
                        .turn
                        .error
                        .as_ref()
                        .filter(|error| !error.message.trim().is_empty());
                    let already_reported =
                        self.last_non_retry_error
                            .as_ref()
                            .is_some_and(|(turn_id, message)| {
                                turn_id == &notification.turn.id
                                    && detailed.is_none_or(|error| message == &error.message)
                            });
                    if !already_reported {
                        let message = detailed.map(|error| error.message.as_str()).unwrap_or("Turn failed. No error details were provided; partial output was retained.");
                        eprintln!("{} {}", "ERROR:".style(self.red).style(self.bold), message);
                        self.last_non_retry_error =
                            Some((notification.turn.id.clone(), message.to_string()));
                    }
                    CodexStatus::InitiateShutdown
                }
                TurnStatus::Interrupted => {
                    self.final_message = None;
                    self.final_message_rendered = false;
                    self.emit_final_message_on_shutdown = false;
                    eprintln!("{}", "turn interrupted".style(self.dimmed));
                    CodexStatus::InitiateShutdown
                }
                TurnStatus::InProgress => CodexStatus::Running,
            },
            ServerNotification::TurnDiffUpdated(notification) => {
                if !notification.diff.trim().is_empty() {
                    eprintln!("{}", notification.diff);
                }
                CodexStatus::Running
            }
            ServerNotification::TurnPlanUpdated(notification) => {
                if let Some(explanation) = notification.explanation {
                    eprintln!("{}", explanation.style(self.italic));
                }
                for step in notification.plan {
                    match step.status {
                        codex_app_server_protocol::TurnPlanStepStatus::Completed => {
                            eprintln!("  {} {}", "✓".style(self.green), step.step);
                        }
                        codex_app_server_protocol::TurnPlanStepStatus::InProgress => {
                            eprintln!("  {} {}", "→".style(self.cyan), step.step);
                        }
                        codex_app_server_protocol::TurnPlanStepStatus::Pending => {
                            eprintln!(
                                "  {} {}",
                                "•".style(self.dimmed),
                                step.step.style(self.dimmed)
                            );
                        }
                    }
                }
                CodexStatus::Running
            }
            ServerNotification::TurnStarted(_) => {
                self.displayed_warnings.clear();
                self.streamed_final_message = Default::default();
                self.last_non_retry_error = None;
                CodexStatus::Running
            }
            _ => CodexStatus::Running,
        }
    }

    fn process_warning(&mut self, message: String) -> CodexStatus {
        if self.displayed_warnings.contains(&message) {
            return CodexStatus::Running;
        }
        // Bound both entry count and retained bytes. Oversize diagnostics still
        // print; they simply do not participate in replay deduplication.
        if message.len() <= 16 * 1024 {
            if self.displayed_warnings.len() == 64 {
                self.displayed_warnings.pop_front();
            }
            self.displayed_warnings.push_back(message.clone());
        }
        eprintln!(
            "{} {message}",
            "warning:".style(self.yellow).style(self.bold)
        );
        CodexStatus::Running
    }

    fn process_event_stream_error(&mut self, message: String) {
        self.final_message = None;
        self.final_message_rendered = false;
        self.emit_final_message_on_shutdown = false;
        eprintln!("{} {message}", "ERROR:".style(self.red).style(self.bold));
    }

    fn print_final_output(&mut self) -> std::io::Result<()> {
        let output_file_result = if self.emit_final_message_on_shutdown
            && let Some(path) = self.last_message_path.as_deref()
        {
            handle_last_message(self.final_message.as_deref(), path)
        } else {
            Ok(())
        };

        if let Some(usage) = &self.last_total_token_usage {
            eprintln!("{}", format_token_usage(usage).style(self.dimmed));
        }

        let mut stdout_result = Ok(());
        if should_print_final_message_to_stdout(
            self.emit_final_message_on_shutdown
                .then_some(self.final_message.as_deref())
                .flatten(),
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
        ) && let Some(message) = self.final_message.as_deref()
        {
            stdout_result = write_final_message(&mut std::io::stdout().lock(), message);
        } else if should_print_final_message_to_tty(
            self.emit_final_message_on_shutdown
                .then_some(self.final_message.as_deref())
                .flatten(),
            self.final_message_rendered,
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
        ) && let Some(message) = self.final_message.as_deref()
        {
            eprintln!(
                "{}\n{}",
                "codex".style(self.italic).style(self.magenta),
                message
            );
        }
        output_file_result.and(stdout_result)
    }
}

/// Writes the final answer to stdout. Unlike `println!`, a closed reader
/// (for example `codex exec ... | head`) becomes an error exit instead of a panic.
fn write_final_message(output: &mut impl std::io::Write, message: &str) -> std::io::Result<()> {
    writeln!(output, "{message}")
        .and_then(|()| output.flush())
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("failed to write final message to stdout: {error}"),
            )
        })
}

fn config_summary_entries(
    config: &Config,
    session_configured_event: &SessionConfiguredEvent,
) -> Vec<(&'static str, String)> {
    let mut entries = vec![
        (
            "workdir",
            session_configured_event.cwd.display().to_string(),
        ),
        ("model", session_configured_event.model.clone()),
        (
            "provider",
            session_configured_event.model_provider_id.clone(),
        ),
        (
            "approval",
            session_configured_event.approval_policy.to_string(),
        ),
        (
            "sandbox",
            summarize_permission_profile(
                &session_configured_event.permission_profile,
                &session_configured_event.cwd,
                config.effective_workspace_roots().as_slice(),
            ),
        ),
    ];
    if config.model_provider.wire_api == WireApi::Responses {
        entries.push((
            "reasoning effort",
            session_configured_event
                .reasoning_effort
                .as_ref()
                .map(std::string::ToString::to_string)
                .unwrap_or_else(|| "none".to_string()),
        ));
        entries.push((
            "reasoning summaries",
            config
                .model_reasoning_summary
                .map(|summary| summary.to_string())
                .unwrap_or_else(|| "none".to_string()),
        ));
    }
    entries.push((
        "session id",
        session_configured_event.session_id.to_string(),
    ));
    entries
}

fn reasoning_text(
    summary: &[String],
    content: &[String],
    show_raw_agent_reasoning: bool,
) -> Option<String> {
    let entries = if show_raw_agent_reasoning && !content.is_empty() {
        content
    } else {
        summary
    };
    if entries.is_empty() {
        None
    } else {
        Some(entries.join("\n"))
    }
}

fn format_token_usage(usage: &ThreadTokenUsage) -> String {
    let usage: codex_protocol::protocol::TokenUsage = usage.total.clone().into();
    FinalOutput::from(usage).to_string()
}

fn should_print_final_message_to_stdout(
    final_message: Option<&str>,
    stdout_is_terminal: bool,
    stderr_is_terminal: bool,
) -> bool {
    final_message.is_some() && !(stdout_is_terminal && stderr_is_terminal)
}

fn should_print_final_message_to_tty(
    final_message: Option<&str>,
    final_message_rendered: bool,
    stdout_is_terminal: bool,
    stderr_is_terminal: bool,
) -> bool {
    final_message.is_some() && !final_message_rendered && stdout_is_terminal && stderr_is_terminal
}

#[cfg(test)]
#[path = "event_processor_with_human_output_tests.rs"]
mod tests;
