use std::collections::BTreeSet;

use codex_tools::ToolFailureClass;
use codex_tools::ToolFailureDiagnostic;
use sha2::Digest;
use sha2::Sha256;

const DEFAULT_SUMMARY_AFTER_BYTES: usize = 48 * 1024;
const DEFAULT_SUMMARY_AFTER_LINES: usize = 600;
const EARLY_SUMMARY_AFTER_BYTES: usize = 10 * 1024;
const EARLY_SUMMARY_AFTER_LINES: usize = 160;
const SUMMARY_MAX_BYTES: usize = 32 * 1024;
const SUMMARY_MAX_LINES: usize = 240;
const SUCCESS_HEAD_LINES: usize = 24;
const SUCCESS_TAIL_LINES: usize = 64;
const FAILURE_TAIL_LINES: usize = 140;
const FOCUS_CONTEXT_LINES: usize = 3;
const MAX_FOCUS_MATCHES: usize = 48;

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
    let critical_signals = critical_signal_indexes(&lines);
    let mut critical_context = BTreeSet::new();
    add_context_ranges(
        lines.len(),
        critical_signals.iter().copied(),
        &mut critical_context,
    );
    let tail_count = if failed || validation {
        FAILURE_TAIL_LINES
    } else {
        SUCCESS_TAIL_LINES
    };
    let tail_start = lines.len().saturating_sub(tail_count);
    let tail_indexes = (tail_start..lines.len()).collect::<BTreeSet<_>>();
    let mut priority_indexes = critical_context.clone();
    priority_indexes.extend(tail_indexes.iter().copied());
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

    // Emit exact failure signals, their context, and the final status tail
    // before advisory ranges. A source-ordered warning flood could otherwise
    // consume the bounded summary before either actionable region.
    let ordered = critical_signals
        .iter()
        .copied()
        .chain(critical_context.difference(&critical_signals).copied())
        .chain(tail_indexes.difference(&critical_context).copied())
        .chain(selected.difference(&priority_indexes).copied());
    let mut previous = None;
    for index in ordered {
        if let Some(previous_index) = previous
            && index != previous_index + 1
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

pub(crate) fn normalized_command_failure_diagnostic(
    output: &str,
    command: Option<&str>,
    exit_code: Option<i32>,
    still_running: bool,
) -> Option<ToolFailureDiagnostic> {
    if !still_running && exit_code.is_none_or(|code| code == 0) {
        return None;
    }

    let command = command.unwrap_or_default();
    let lower_command = command.to_ascii_lowercase();
    let class = if lower_command.contains("test") || lower_command.contains("pytest") {
        ToolFailureClass::Test
    } else if [
        "cargo build",
        "cargo check",
        "cargo clippy",
        "npm run build",
        "pnpm build",
        "typecheck",
    ]
    .iter()
    .any(|needle| lower_command.contains(needle))
        || output.contains("error[")
    {
        ToolFailureClass::Compiler
    } else {
        ToolFailureClass::Runtime
    };
    let owner_hint = command_failure_owner(output).unwrap_or("command execution");
    let first_signal = output
        .lines()
        .find(|line| is_critical_failure_signal(line))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .unwrap_or(if still_running {
            "command is still running"
        } else {
            "command exited unsuccessfully"
        });
    let normalized_signal = normalize_failure_signal(first_signal);
    let normalized_owner = normalize_owner_identity(owner_hint);
    let fingerprint = format!(
        "command.{:?}.{:x}",
        class,
        Sha256::digest(format!("{normalized_owner}\0{normalized_signal}").as_bytes())
    )
    .to_ascii_lowercase();
    let next_action = match class {
        ToolFailureClass::Compiler => {
            format!(
                "inspect `{owner_hint}` first, then rerun the same compiler check after a relevant change"
            )
        }
        ToolFailureClass::Test => {
            format!(
                "inspect `{owner_hint}` and rerun only the failing test selector after a relevant change"
            )
        }
        ToolFailureClass::Runtime => {
            if still_running {
                "poll the existing session instead of launching an equivalent command".to_string()
            } else {
                format!(
                    "inspect `{owner_hint}` and change the input or owning implementation before retrying"
                )
            }
        }
        _ => unreachable!("command failures use compiler, test, or runtime classes"),
    };

    Some(
        ToolFailureDiagnostic::model_visible(
            class,
            fingerprint,
            format!("command failure routed to `{owner_hint}`: {first_signal}"),
        )
        .with_retryable(still_running)
        .with_owner_hint(owner_hint)
        .with_next_action(next_action),
    )
}

fn normalize_owner_identity(owner: &str) -> &str {
    let Some((without_column, column)) = owner.rsplit_once(':') else {
        return owner;
    };
    if !column.chars().all(|character| character.is_ascii_digit()) {
        return owner;
    }
    let Some((path, line)) = without_column.rsplit_once(':') else {
        return owner;
    };
    if line.chars().all(|character| character.is_ascii_digit()) {
        path
    } else {
        owner
    }
}

fn command_failure_owner(output: &str) -> Option<&str> {
    output.lines().find_map(|line| {
        let trimmed = line.trim();
        if let Some(location) = trimmed.strip_prefix("-->") {
            return Some(location.trim());
        }
        if trimmed.contains("::") && (trimmed.contains("FAILED") || trimmed.contains("ERROR")) {
            return trimmed.split_whitespace().next();
        }
        if let Some(test) = trimmed.strip_prefix("test ")
            && let Some((name, status)) = test.rsplit_once(' ')
            && matches!(status, "FAILED" | "failed")
        {
            return Some(name.trim());
        }
        None
    })
}

fn normalize_failure_signal(signal: &str) -> String {
    signal
        .split_whitespace()
        .map(|part| {
            if part.chars().all(|character| character.is_ascii_digit()) {
                "#"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    let critical = critical_signal_indexes(lines);
    add_context_ranges(lines.len(), critical.iter().copied(), selected);
    let remaining = MAX_FOCUS_MATCHES.saturating_sub(critical.len());
    let advisory = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| is_advisory_signal(line).then_some(index));
    add_context_ranges(lines.len(), advisory.take(remaining), selected);
}

fn critical_signal_indexes(lines: &[&str]) -> BTreeSet<usize> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| is_critical_failure_signal(line).then_some(index))
        .take(MAX_FOCUS_MATCHES)
        .collect()
}

fn add_context_ranges(
    line_count: usize,
    indexes: impl Iterator<Item = usize>,
    selected: &mut BTreeSet<usize>,
) {
    for index in indexes {
        let start = index.saturating_sub(FOCUS_CONTEXT_LINES);
        let end = (index + FOCUS_CONTEXT_LINES + 1).min(line_count);
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

fn is_critical_failure_signal(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error")
        || lower.contains("failed")
        || lower.contains("failure")
        || lower.contains("panic")
        || lower.contains("thread ")
        || lower.contains("expected")
        || lower.contains("actual")
        || lower.contains("error[")
}

fn is_advisory_signal(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("warning")
        || lower.contains("warning[")
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
        if self.lines + 1 > SUMMARY_MAX_LINES {
            self.capped = true;
            return;
        }

        let separator_bytes = usize::from(!self.text.is_empty());
        let remaining = SUMMARY_MAX_BYTES
            .saturating_sub(self.text.len())
            .saturating_sub(separator_bytes);
        if remaining == 0 {
            self.capped = true;
            return;
        }
        let rendered = if line.len() > remaining {
            self.capped = true;
            summarize_oversized_line(line, remaining)
        } else {
            line.to_string()
        };

        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(&rendered);
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

fn summarize_oversized_line(line: &str, max_bytes: usize) -> String {
    const MARKER: &str = " ... [line truncated] ... ";
    if line.len() <= max_bytes {
        return line.to_string();
    }
    if max_bytes <= MARKER.len() {
        return take_prefix_at_char_boundary(line, max_bytes).to_string();
    }

    let payload_bytes = max_bytes - MARKER.len();
    let head_bytes = payload_bytes / 2;
    let tail_bytes = payload_bytes - head_bytes;
    let head = take_prefix_at_char_boundary(line, head_bytes);
    let tail = take_suffix_at_char_boundary(line, tail_bytes);
    format!("{head}{MARKER}{tail}")
}

fn take_prefix_at_char_boundary(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn take_suffix_at_char_boundary(value: &str, max_bytes: usize) -> &str {
    let mut start = value.len().saturating_sub(max_bytes);
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

#[cfg(test)]
#[path = "shell_output_summary_tests.rs"]
mod tests;
