use std::collections::BTreeSet;

use codex_config::coding_budgets::SHELL_OUTPUT_DEFAULT_SUMMARY_AFTER_BYTES;
use codex_config::coding_budgets::SHELL_OUTPUT_DEFAULT_SUMMARY_AFTER_LINES;
use codex_config::coding_budgets::SHELL_OUTPUT_EARLY_SUMMARY_AFTER_BYTES;
use codex_config::coding_budgets::SHELL_OUTPUT_EARLY_SUMMARY_AFTER_LINES;
use codex_config::coding_budgets::SHELL_OUTPUT_FAILURE_TAIL_LINES;
use codex_config::coding_budgets::SHELL_OUTPUT_FOCUS_CONTEXT_LINES;
use codex_config::coding_budgets::SHELL_OUTPUT_MAX_FOCUS_MATCHES;
use codex_config::coding_budgets::SHELL_OUTPUT_SUCCESS_HEAD_LINES;
use codex_config::coding_budgets::SHELL_OUTPUT_SUCCESS_TAIL_LINES;
use codex_config::coding_budgets::SHELL_OUTPUT_SUMMARY_MAX_BYTES;
use codex_config::coding_budgets::SHELL_OUTPUT_SUMMARY_MAX_LINES;

const DEFAULT_SUMMARY_AFTER_BYTES: usize = SHELL_OUTPUT_DEFAULT_SUMMARY_AFTER_BYTES;
const DEFAULT_SUMMARY_AFTER_LINES: usize = SHELL_OUTPUT_DEFAULT_SUMMARY_AFTER_LINES;
const EARLY_SUMMARY_AFTER_BYTES: usize = SHELL_OUTPUT_EARLY_SUMMARY_AFTER_BYTES;
const EARLY_SUMMARY_AFTER_LINES: usize = SHELL_OUTPUT_EARLY_SUMMARY_AFTER_LINES;
const SUMMARY_MAX_BYTES: usize = SHELL_OUTPUT_SUMMARY_MAX_BYTES;
const SUMMARY_MAX_LINES: usize = SHELL_OUTPUT_SUMMARY_MAX_LINES;
const SUCCESS_HEAD_LINES: usize = SHELL_OUTPUT_SUCCESS_HEAD_LINES;
const SUCCESS_TAIL_LINES: usize = SHELL_OUTPUT_SUCCESS_TAIL_LINES;
const FAILURE_TAIL_LINES: usize = SHELL_OUTPUT_FAILURE_TAIL_LINES;
const FOCUS_CONTEXT_LINES: usize = SHELL_OUTPUT_FOCUS_CONTEXT_LINES;
const MAX_FOCUS_MATCHES: usize = SHELL_OUTPUT_MAX_FOCUS_MATCHES;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ShellOutputSummaryOptions<'a> {
    pub(crate) enabled: bool,
    /// This only lowers the model-visible summarization threshold. It must never
    /// block, rewrite, deny, reroute, or otherwise alter command execution.
    pub(crate) turn_cost_guard: bool,
    /// Optional command text may classify output shape, such as validation/build
    /// output. Do not add extra plumbing just to carry this value.
    pub(crate) command_text: Option<&'a str>,
}

pub(crate) fn summarize_shell_output_for_model(
    output: &str,
    exit_code: i32,
    timed_out: bool,
    options: ShellOutputSummaryOptions<'_>,
) -> Option<String> {
    if !options.enabled {
        return None;
    }

    let line_count = output.lines().count();
    let (byte_threshold, line_threshold) = if options.turn_cost_guard {
        (EARLY_SUMMARY_AFTER_BYTES, EARLY_SUMMARY_AFTER_LINES)
    } else {
        (DEFAULT_SUMMARY_AFTER_BYTES, DEFAULT_SUMMARY_AFTER_LINES)
    };
    if output.len() <= byte_threshold && line_count <= line_threshold {
        return None;
    }

    let lines = output.lines().collect::<Vec<_>>();
    let failed = timed_out || exit_code != 0;
    let validation = options
        .command_text
        .is_some_and(looks_like_validation_command);
    let selected = selected_line_indexes(&lines, failed, validation);
    let retained_shape = if validation {
        "failure-focused lines, final status lines, tail"
    } else if failed {
        "failure-focused lines, tail"
    } else {
        "head, warning/error lines, tail"
    };

    let mut builder = SummaryBuilder::new();
    builder.push_line("Shell output summary:");
    builder.push_line(format!("- original_lines: {line_count}"));
    builder.push_line(format!("- original_bytes: {}", output.len()));
    builder.push_line(format!("- exit_code: {exit_code}"));
    if timed_out {
        builder.push_line("- timed_out: true");
    }
    builder.push_line(format!("- retained: {retained_shape}"));
    builder.push_line("");
    builder.push_line("Selected output lines:");

    let mut previous = None;
    for index in selected {
        if let Some(previous_index) = previous
            && index > previous_index + 1
        {
            builder.push_line("...");
        }
        if let Some(line) = lines.get(index) {
            builder.push_line(format!("{:>5}: {line}", index + 1));
        }
        previous = Some(index);
        if builder.is_full() {
            break;
        }
    }
    builder.finish()
}

fn selected_line_indexes(lines: &[&str], failed: bool, validation: bool) -> BTreeSet<usize> {
    let mut selected = BTreeSet::new();
    if failed || validation {
        add_focus_ranges(lines, &mut selected);
        add_status_lines(lines, &mut selected);
        if selected.is_empty() {
            add_head(lines, &mut selected, SUCCESS_HEAD_LINES);
        }
        add_tail(lines, &mut selected, FAILURE_TAIL_LINES);
    } else {
        add_head(lines, &mut selected, SUCCESS_HEAD_LINES);
        add_focus_ranges(lines, &mut selected);
        add_tail(lines, &mut selected, SUCCESS_TAIL_LINES);
    }
    selected
}

fn add_head(lines: &[&str], selected: &mut BTreeSet<usize>, count: usize) {
    for index in 0..lines.len().min(count) {
        selected.insert(index);
    }
}

fn add_tail(lines: &[&str], selected: &mut BTreeSet<usize>, count: usize) {
    let start = lines.len().saturating_sub(count);
    for index in start..lines.len() {
        selected.insert(index);
    }
}

fn add_focus_ranges(lines: &[&str], selected: &mut BTreeSet<usize>) {
    for index in lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| is_failure_signal(line).then_some(index))
        .take(MAX_FOCUS_MATCHES)
    {
        let start = index.saturating_sub(FOCUS_CONTEXT_LINES);
        let end = (index + FOCUS_CONTEXT_LINES + 1).min(lines.len());
        for selected_index in start..end {
            selected.insert(selected_index);
        }
    }
}

fn add_status_lines(lines: &[&str], selected: &mut BTreeSet<usize>) {
    for index in lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| is_final_status_signal(line).then_some(index))
        .take(MAX_FOCUS_MATCHES)
    {
        selected.insert(index);
    }
}

fn is_failure_signal(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error")
        || lower.contains("failed")
        || lower.contains("failure")
        || lower.contains("panic")
        || lower.contains("thread ")
        || lower.contains("expected")
        || lower.contains("actual")
        || lower.contains("warning")
        || lower.contains("warning[")
        || lower.contains("error[")
        || lower.trim_start().starts_with("-->")
        || lower.trim_start().starts_with("note:")
        || lower.trim_start().starts_with("help:")
}

fn is_final_status_signal(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("test result:")
        || lower.contains("failures:")
        || lower.contains("failed.")
        || lower.contains("passed")
        || lower.contains("finished ")
        || lower.contains("error:")
        || lower.contains("summary:")
}

fn looks_like_validation_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "cargo build",
        "cargo check",
        "cargo clippy",
        "cargo test",
        "cargo nextest",
        "just test",
        "just test-fast",
        "just check",
        "just fix",
        "npm test",
        "npm run build",
        "npm run lint",
        "npm run typecheck",
        "pnpm test",
        "pnpm build",
        "pnpm lint",
        "pnpm typecheck",
        "pytest",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

struct SummaryBuilder {
    text: String,
    lines: usize,
    capped: bool,
}

impl SummaryBuilder {
    fn new() -> Self {
        Self {
            text: String::new(),
            lines: 0,
            capped: false,
        }
    }

    fn push_line(&mut self, line: impl AsRef<str>) {
        if self.is_full() {
            self.capped = true;
            return;
        }

        let line = line.as_ref();
        let additional_bytes = line.len() + usize::from(!self.text.is_empty());
        if self.lines + 1 > SUMMARY_MAX_LINES
            || self.text.len().saturating_add(additional_bytes) > SUMMARY_MAX_BYTES
        {
            self.capped = true;
            return;
        }

        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(line);
        self.lines += 1;
    }

    fn is_full(&self) -> bool {
        self.lines >= SUMMARY_MAX_LINES || self.text.len() >= SUMMARY_MAX_BYTES
    }

    fn finish(mut self) -> Option<String> {
        if self.text.trim().is_empty() {
            return None;
        }
        if self.capped && !self.text.ends_with("[summary capped]") {
            if !self.text.is_empty() {
                self.text.push('\n');
            }
            self.text.push_str("[summary capped]");
        }
        Some(self.text)
    }
}

#[cfg(test)]
#[path = "shell_output_summary_tests.rs"]
mod tests;
