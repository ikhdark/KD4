use crate::tools::handlers::command_shape::CommandInvocation;
use crate::validation_admission::ValidationClassification;
use crate::validation_admission::classify_validation;
use std::collections::BTreeSet;
use std::collections::VecDeque;

const DEFAULT_SUMMARY_AFTER_BYTES: usize = 48 * 1024;
const DEFAULT_SUMMARY_AFTER_LINES: usize = 600;
const SUMMARY_MAX_BYTES: usize = 32 * 1024;
const SUMMARY_MAX_LINES: usize = 240;
// Reserve room for the retention counts and an explicit cap marker.
const SUMMARY_FOOTER_BYTES: usize = 128;
const SUMMARY_FOOTER_LINES: usize = 3;
const SUCCESS_HEAD_LINES: usize = 24;
const SUCCESS_TAIL_LINES: usize = 64;
const VALIDATION_SUCCESS_TAIL_LINES: usize = 16;
const FAILURE_TAIL_LINES: usize = 140;
const FOCUS_CONTEXT_LINES: usize = 3;
// Keep the selected ranges below the summary line ceiling even when every
// match is disjoint and receives the full context window. This leaves room for
// the failure tail, final statuses, separators, and summary metadata.
const MAX_FOCUS_MATCHES: usize = 8;
const MAX_STATUS_MATCHES: usize = 8;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ShellOutputSummaryOptions<'a> {
    pub(crate) enabled: bool,
    /// Summarize when output exceeds the actual projection budget, including
    /// outputs below the default large-output threshold.
    pub(crate) applied_token_limit: Option<usize>,
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
    if options.command_text.is_some_and(|command| {
        let commands = codex_shell_command::parse_command::parse_shell_script(command);
        !commands.is_empty()
            && commands.iter().all(|command| {
                matches!(
                    command,
                    codex_protocol::parse_command::ParsedCommand::Read { .. }
                        | codex_protocol::parse_command::ParsedCommand::Search { .. }
                        | codex_protocol::parse_command::ParsedCommand::ListFiles { .. }
                )
            })
    }) {
        // Preserve the requested source order and the existing truncation/raw
        // artifact recovery path instead of ranking code as diagnostic prose.
        return None;
    }

    let exceeds_token_budget = options
        .applied_token_limit
        .is_some_and(|limit| codex_utils_string::approx_token_count(output) > limit);
    if !exceeds_token_budget
        && output.len() <= DEFAULT_SUMMARY_AFTER_BYTES
        && output.lines().take(DEFAULT_SUMMARY_AFTER_LINES + 1).count()
            <= DEFAULT_SUMMARY_AFTER_LINES
    {
        return None;
    }
    let line_count = output.lines().count();
    let failed = timed_out || exit_code != 0;
    let validation = options.command_text.is_some_and(|command| {
        matches!(
            classify_validation(&CommandInvocation::Script(command.to_string())),
            ValidationClassification::Validation { leaves, .. } if !leaves.is_empty()
        )
    });
    let selection = select_lines(output, line_count, failed, validation);
    let selection_policy = if validation {
        "source-ordered failure-focused lines, final status lines, tail"
    } else if failed {
        "source-ordered failure-focused lines, tail"
    } else {
        "source-ordered head, warning/error lines, tail"
    };

    let mut builder = SummaryBuilder::new();
    builder.push_line("Shell output summary:");
    builder.push_line(format!("- original_lines: {line_count}"));
    builder.push_line(format!("- original_bytes: {}", output.len()));
    builder.push_line(format!("- exit_code: {exit_code}"));
    if timed_out {
        builder.push_line("- timed_out: true");
    }
    builder.push_line(format!("- selection_policy: {selection_policy}"));
    if selection.omitted_groups > 0 {
        let qualifier = if selection.groups_overflowed {
            "at least "
        } else {
            ""
        };
        builder.push_line(format!("- omitted_diagnostic_groups: {qualifier}{}; inspect the raw output for remaining diagnostics", selection.omitted_groups));
    }
    builder.push_line("");
    builder.push_line("Selected output lines:");

    // Candidate quotas reserve space for actionable and final regions before
    // rendering. Emit the selected lines in source order so a diagnostic stays
    // attached to the context that explains it.
    // Borrow only the bounded selection; never copy oversized source lines.
    let selected = output
        .lines()
        .enumerate()
        .filter(|(index, _)| selection.indexes.contains(index))
        .collect::<Vec<_>>();
    let gap_bytes: usize = selected
        .windows(2)
        .filter_map(|pair| {
            (pair[1].0 != pair[0].0 + 1)
                .then(|| format!("\n... [{} lines omitted]", pair[1].0 - pair[0].0 - 1).len())
        })
        .sum();
    let prefixes: usize = selected
        .iter()
        .map(|(index, _)| format!("{:>5}: ", index + 1).len())
        .sum();
    let available = SUMMARY_MAX_BYTES.saturating_sub(
        SUMMARY_FOOTER_BYTES + builder.text.len() + gap_bytes + prefixes + selected.len(),
    );
    let mut low = 0;
    let mut high = available;
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if selected
            .iter()
            .map(|(_, line)| line.len().min(mid))
            .sum::<usize>()
            <= available
        {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let line_budget = low;
    let mut previous = None;
    let mut emitted_source_lines = 0;
    for (index, line) in selected {
        let before_gap = (builder.text.len(), builder.lines);
        if let Some(previous_index) = previous
            && index != previous_index + 1
            && !builder.push_line(format!(
                "... [{} lines omitted]",
                index - previous_index - 1
            ))
        {
            builder.text.truncate(before_gap.0);
            builder.lines = before_gap.1;
            break;
        }
        {
            builder.capped |= line.len() > line_budget;
            let line = summarize_oversized_line(line, line_budget);
            if !builder.push_line(format!("{:>5}: {line}", index + 1)) {
                // A gap describes the next retained line. Keep the pair atomic
                // when the gap uses the last available byte or line slot.
                builder.text.truncate(before_gap.0);
                builder.lines = before_gap.1;
                break;
            }
            emitted_source_lines += 1;
        }
        previous = Some(index);
    }
    builder
        .finish(emitted_source_lines, line_count)
        .filter(|summary| summary.len() < output.len())
}

#[derive(Clone, Copy, Default)]
struct LineClassification {
    critical: bool,
    advisory: bool,
    status: bool,
}

fn classify_line(line: &str) -> LineClassification {
    let trimmed = line.trim_start();
    LineClassification {
        critical: starts_with_diagnostic_label_ascii_case(trimmed, "error")
            || starts_with_diagnostic_label_ascii_case(trimmed, "failed")
            || starts_with_diagnostic_label_ascii_case(trimmed, "failure")
            || starts_with_diagnostic_label_ascii_case(trimmed, "panic")
            || starts_with_diagnostic_label_ascii_case(trimmed, "fatal")
            || starts_with_ascii_case(trimmed, "fail [")
            || strip_prefix_ascii_case(trimmed, "try ").is_some_and(|retry| {
                find_ascii_case(retry, " fail [").is_some_and(|separator| {
                    let attempt = &retry[..separator];
                    !attempt.is_empty() && attempt.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
            || starts_with_ascii_case(trimmed, "failures:")
            // Failed suites must survive later passing status lines in a
            // multi-package run, even when their detailed errors were omitted.
            || contains_ascii_case(line, "test result: failed")
            || starts_with_ascii_case(trimmed, "panicked at ")
            || contains_ascii_case(line, " panicked at ")
            || contains_ascii_case(line, " error:")
            // TypeScript diagnostics use "error TS<digits>:" after a location.
            || find_ascii_case(line, "error ts").is_some_and(|start| {
                line[start + "error ts".len()..].split_once(':').is_some_and(|(number, _)| {
                    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
            || contains_ascii_case(line, "npm err!"),
        advisory: contains_word_ascii_case(line, "warning")
            || trimmed.starts_with("-->")
            || starts_with_ascii_case(trimmed, "note:")
            || starts_with_ascii_case(trimmed, "help:"),
        status: contains_ascii_case(line, "test result:")
            || contains_ascii_case(line, "failures:")
            || contains_ascii_case(line, "failed.")
            || (contains_word_ascii_case(line, "passed")
                && (starts_with_ascii_case(trimmed, "test ")
                    || starts_with_ascii_case(trimmed, "tests ")
                    || starts_with_ascii_case(trimmed, "suite ")
                    || find_ascii_case(trimmed, " passed").is_some_and(|separator| {
                        let count = &trimmed[..separator];
                        !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit())
                    })))
            || contains_ascii_case(line, "finished ")
            || starts_with_diagnostic_label_ascii_case(trimmed, "error")
            || starts_with_ascii_case(trimmed, "summary:")
            || starts_with_ascii_case(trimmed, "summary ["),
    }
}

fn contains_word_ascii_case(line: &str, word: &str) -> bool {
    line.split(|character: char| {
        !character.is_alphanumeric() && character != '_' && character != '-'
    })
    .any(|candidate| candidate.eq_ignore_ascii_case(word))
}

fn starts_with_diagnostic_label_ascii_case(line: &str, label: &str) -> bool {
    strip_prefix_ascii_case(line, label).is_some_and(|remainder| {
        matches!(
            remainder.as_bytes().first(),
            None | Some(b':') | Some(b'[') | Some(b'.') | Some(b' ')
        )
    })
}

fn starts_with_ascii_case(line: &str, prefix: &str) -> bool {
    strip_prefix_ascii_case(line, prefix).is_some()
}

fn strip_prefix_ascii_case<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    line.as_bytes()
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix.as_bytes()))
        .then(|| &line[prefix.len()..])
}

fn contains_ascii_case(line: &str, needle: &str) -> bool {
    find_ascii_case(line, needle).is_some()
}

fn find_ascii_case(line: &str, needle: &str) -> Option<usize> {
    line.as_bytes()
        .windows(needle.len())
        .position(|candidate| candidate.eq_ignore_ascii_case(needle.as_bytes()))
}

// The selector retains at most 64 diagnostic groups and a fixed number of
// indexes, regardless of log length. Text is borrowed from the captured output.
const MAX_DIAGNOSTIC_GROUPS: usize = 64;

struct LineSelection {
    indexes: BTreeSet<usize>,
    omitted_groups: usize,
    groups_overflowed: bool,
}

fn select_lines(output: &str, line_count: usize, failed: bool, validation: bool) -> LineSelection {
    let mut groups: Vec<((&str, Option<&str>), usize)> = Vec::new();
    let mut groups_overflowed = false;
    let mut lines = output.lines().enumerate().peekable();
    while let Some((index, line)) = lines.next() {
        let location = lines
            .peek()
            .map(|(_, next)| next.trim())
            .filter(|next| next.starts_with("-->"));
        let identity = (line.trim(), location);
        if classify_line(line).critical && !groups.iter().any(|(text, _)| *text == identity) {
            if groups.len() == MAX_DIAGNOSTIC_GROUPS {
                groups_overflowed = true;
                // Retain the latest distinct group as well as the stable prefix.
                groups.pop();
            }
            groups.push((identity, index));
        }
    }
    let group_count = groups.len();
    let critical: Vec<usize> = if group_count <= MAX_FOCUS_MATCHES {
        groups.iter().map(|(_, index)| *index).collect()
    } else {
        groups[..MAX_FOCUS_MATCHES / 2]
            .iter()
            .chain(groups[group_count - MAX_FOCUS_MATCHES / 2..].iter())
            .map(|(_, index)| *index)
            .collect()
    };
    let mut indexes = BTreeSet::new();
    for &index in &critical {
        indexes.extend(
            index.saturating_sub(FOCUS_CONTEXT_LINES)
                ..(index + FOCUS_CONTEXT_LINES + 1).min(line_count),
        );
    }
    let mut advisory_slots = MAX_FOCUS_MATCHES - critical.len();
    let mut statuses = VecDeque::new();
    for (index, line) in output.lines().enumerate() {
        let classification = classify_line(line);
        if classification.advisory && advisory_slots > 0 && !indexes.contains(&index) {
            indexes.extend(
                index.saturating_sub(FOCUS_CONTEXT_LINES)
                    ..(index + FOCUS_CONTEXT_LINES + 1).min(line_count),
            );
            advisory_slots -= 1;
        }
        if (failed || validation) && classification.status && !indexes.contains(&index) {
            if statuses.len() == MAX_STATUS_MATCHES {
                statuses.pop_front();
            }
            statuses.push_back(index);
        }
    }
    indexes.extend(statuses);
    if !(failed || validation) || indexes.is_empty() {
        indexes.extend(0..SUCCESS_HEAD_LINES.min(line_count));
    }
    let tail = if failed {
        FAILURE_TAIL_LINES
    } else if validation {
        VALIDATION_SUCCESS_TAIL_LINES
    } else {
        SUCCESS_TAIL_LINES
    };
    indexes.extend(line_count.saturating_sub(tail)..line_count);
    // Count groups whose representative is absent, including context/tail
    // coverage. After overflow this is a lower bound, never a claim of completeness.
    let omitted_groups = groups
        .iter()
        .filter(|(_, index)| !indexes.contains(index))
        .count()
        + usize::from(groups_overflowed);
    LineSelection {
        indexes,
        omitted_groups,
        groups_overflowed,
    }
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

    fn push_line(&mut self, line: impl AsRef<str>) -> bool {
        if self.is_full() {
            self.capped = true;
            return false;
        }

        let line = line.as_ref();
        let separator_bytes = usize::from(!self.text.is_empty());
        let remaining = SUMMARY_MAX_BYTES
            .saturating_sub(SUMMARY_FOOTER_BYTES)
            .saturating_sub(self.text.len())
            .saturating_sub(separator_bytes);
        if remaining == 0 {
            self.capped = true;
            return false;
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
        true
    }

    fn is_full(&self) -> bool {
        self.lines >= SUMMARY_MAX_LINES - SUMMARY_FOOTER_LINES
            || self.text.len() >= SUMMARY_MAX_BYTES - SUMMARY_FOOTER_BYTES
    }

    fn finish(mut self, emitted_source_lines: usize, original_lines: usize) -> Option<String> {
        if self.text.trim().is_empty() {
            return None;
        }
        self.text.push_str(&format!(
            "\n- emitted_source_lines: {emitted_source_lines}\n- omitted_source_lines: {}",
            original_lines.saturating_sub(emitted_source_lines),
        ));
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
        return line[..line.floor_char_boundary(max_bytes)].to_string();
    }

    let payload_bytes = max_bytes - MARKER.len();
    let head_bytes = payload_bytes / 2;
    let tail_bytes = payload_bytes - head_bytes;
    let head = &line[..line.floor_char_boundary(head_bytes)];
    let tail_start = line.ceil_char_boundary(line.len().saturating_sub(tail_bytes));
    let tail = &line[tail_start..];
    format!("{head}{MARKER}{tail}")
}

#[cfg(test)]
mod optimization_tests {
    use super::*;

    #[test]
    fn selection_storage_is_bounded_for_large_logs() {
        let output = "error: repeated failure\nordinary context\n".repeat(100_000);
        let selection = select_lines(&output, 200_000, true, true);
        assert!(
            selection.indexes.len()
                <= FAILURE_TAIL_LINES + MAX_FOCUS_MATCHES * 7 + MAX_STATUS_MATCHES
        );
        assert!(selection.indexes.contains(&0));
        assert!(selection.indexes.contains(&199_999));
        assert_eq!(selection.omitted_groups, 0);
    }

    #[test]
    fn line_classification_is_case_insensitive_without_retained_scratch() {
        let classification = classify_line("  ERROR: warning; tests PASSED");
        assert!(classification.critical);
        assert!(classification.advisory);
        assert!(classification.status);
        let classification = classify_line("ordinary output");
        assert!(!classification.critical);
        assert!(!classification.advisory);
        assert!(!classification.status);
    }

    #[test]
    fn selection_flags_emit_each_index_once_in_source_order() {
        let lines = [
            "warning: advisory",
            "before",
            "error: exact failure",
            "after one",
            "after two",
            "after three",
            "tail one",
            "tail two",
        ];
        let selection = select_lines(&lines.join("\n"), lines.len(), true, true);
        let ordered = selection.indexes.into_iter().collect::<Vec<_>>();

        assert_eq!(ordered, (0..lines.len()).collect::<Vec<_>>());
        let mut sorted = ordered.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ordered.len());
    }
}

#[cfg(test)]
#[path = "shell_output_summary_tests.rs"]
mod tests;
