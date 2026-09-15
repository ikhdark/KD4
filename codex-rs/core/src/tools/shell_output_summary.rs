use codex_utils_output_truncation::looks_like_validation_command;

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

    let byte_threshold = options
        .applied_token_limit
        .map_or(DEFAULT_SUMMARY_AFTER_BYTES, |limit| {
            codex_utils_string::approx_bytes_for_tokens(limit).min(DEFAULT_SUMMARY_AFTER_BYTES)
        });
    let lines = collect_lines_for_summary(output, byte_threshold, DEFAULT_SUMMARY_AFTER_LINES)?;
    let line_count = lines.len();
    let failed = timed_out || exit_code != 0;
    let validation = options
        .command_text
        .is_some_and(looks_like_validation_command);
    let line_states = select_line_states(&lines, failed, validation);
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
    builder.push_line("");
    builder.push_line("Selected output lines:");

    // Candidate quotas reserve space for actionable and final regions before
    // rendering. Emit the selected lines in source order so a diagnostic stays
    // attached to the context that explains it.
    let ordered = ordered_line_indexes(&line_states);
    let mut previous = None;
    let mut emitted_source_lines = 0;
    for index in ordered {
        if let Some(previous_index) = previous
            && index != previous_index + 1
            && !builder.push_line(format!(
                "... [{} lines omitted]",
                index - previous_index - 1
            ))
        {
            break;
        }
        if let Some(line) = lines.get(index) {
            if !builder.push_line(format!("{:>5}: {line}", index + 1)) {
                break;
            }
            emitted_source_lines += 1;
        }
        previous = Some(index);
    }
    builder.finish(emitted_source_lines, line_count)
}

fn collect_lines_for_summary(
    output: &str,
    byte_threshold: usize,
    line_threshold: usize,
) -> Option<Vec<&str>> {
    if output.len() > byte_threshold {
        return Some(output.lines().collect());
    }
    let mut buffered = Vec::new();
    let mut remaining = output.lines();
    while let Some(line) = remaining.next() {
        buffered.push(line);
        if buffered.len() > line_threshold {
            buffered.extend(remaining);
            return Some(buffered);
        }
    }
    None
}

#[derive(Clone, Copy, Default)]
struct LineClassification {
    critical: bool,
    advisory: bool,
    status: bool,
}

#[derive(Clone, Copy, Default)]
struct LineState {
    classification: LineClassification,
    selected: bool,
}

fn classify_line(line: &str, lower: &mut String) -> LineClassification {
    lower.clear();
    lower.push_str(line);
    lower.make_ascii_lowercase();
    let trimmed = lower.trim_start();
    LineClassification {
        critical: starts_with_diagnostic_label(trimmed, "error")
            || starts_with_diagnostic_label(trimmed, "failed")
            || starts_with_diagnostic_label(trimmed, "failure")
            || starts_with_diagnostic_label(trimmed, "panic")
            || starts_with_diagnostic_label(trimmed, "fatal")
            || trimmed.starts_with("fail [")
            || trimmed.strip_prefix("try ").is_some_and(|retry| {
                retry.split_once(" fail [").is_some_and(|(attempt, _)| {
                    !attempt.is_empty() && attempt.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
            || trimmed.starts_with("failures:")
            || trimmed.starts_with("panicked at ")
            || lower.contains(" panicked at ")
            || lower.contains(" error:")
            // TypeScript diagnostics use "error TS<digits>:" after a location.
            || lower.split_once("error ts").is_some_and(|(_, code)| {
                code.split_once(':').is_some_and(|(number, _)| {
                    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
            || lower.contains("npm err!"),
        advisory: contains_word(lower, "warning")
            || trimmed.starts_with("-->")
            || trimmed.starts_with("note:")
            || trimmed.starts_with("help:"),
        status: lower.contains("test result:")
            || lower.contains("failures:")
            || lower.contains("failed.")
            || (contains_word(lower, "passed")
                && (trimmed.starts_with("test ")
                    || trimmed.starts_with("tests ")
                    || trimmed.starts_with("suite ")
                    || trimmed.split_once(" passed").is_some_and(|(count, _)| {
                        !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit())
                    })))
            || lower.contains("finished ")
            || starts_with_diagnostic_label(trimmed, "error")
            || lower.contains("summary:")
            || trimmed.starts_with("summary ["),
    }
}

fn contains_word(line: &str, word: &str) -> bool {
    line.split(|character: char| {
        !character.is_alphanumeric() && character != '_' && character != '-'
    })
    .any(|candidate| candidate == word)
}

fn starts_with_diagnostic_label(line: &str, label: &str) -> bool {
    line.strip_prefix(label).is_some_and(|remainder| {
        matches!(
            remainder.as_bytes().first(),
            None | Some(b':') | Some(b'[') | Some(b'.') | Some(b' ')
        )
    })
}

fn select_line_states(lines: &[&str], failed: bool, validation: bool) -> Vec<LineState> {
    let mut lowercase = String::new();
    let mut states = lines
        .iter()
        .map(|line| LineState {
            classification: classify_line(line, &mut lowercase),
            ..LineState::default()
        })
        .collect::<Vec<_>>();
    if failed || validation {
        add_focus_ranges(&mut states);
        add_status_lines(&mut states);
        if !states.iter().any(|state| state.selected) {
            add_head(&mut states, SUCCESS_HEAD_LINES);
        }
        add_tail(
            &mut states,
            if failed {
                FAILURE_TAIL_LINES
            } else {
                VALIDATION_SUCCESS_TAIL_LINES
            },
        );
    } else {
        add_head(&mut states, SUCCESS_HEAD_LINES);
        add_focus_ranges(&mut states);
        add_tail(&mut states, SUCCESS_TAIL_LINES);
    }
    states
}

fn add_head(states: &mut [LineState], count: usize) {
    for state in states.iter_mut().take(count) {
        state.selected = true;
    }
}

fn add_tail(states: &mut [LineState], count: usize) {
    let start = states.len().saturating_sub(count);
    for state in &mut states[start..] {
        state.selected = true;
    }
}

fn add_focus_ranges(states: &mut [LineState]) {
    let all_critical = states
        .iter()
        .enumerate()
        .filter_map(|(index, state)| state.classification.critical.then_some(index))
        .collect::<Vec<_>>();
    let critical = bounded_edge_indexes(&all_critical, MAX_FOCUS_MATCHES);
    add_context_ranges(states, &critical);
    let remaining = MAX_FOCUS_MATCHES.saturating_sub(critical.len());
    let advisory = states
        .iter()
        .enumerate()
        .filter_map(|(index, state)| {
            (state.classification.advisory && !state.selected).then_some(index)
        })
        .take(remaining)
        .collect::<Vec<_>>();
    add_context_ranges(states, &advisory);
}

fn bounded_edge_indexes(indexes: &[usize], limit: usize) -> Vec<usize> {
    if indexes.len() <= limit {
        return indexes.to_vec();
    }
    let head = limit.div_ceil(2);
    let tail = limit.saturating_sub(head);
    indexes[..head]
        .iter()
        .chain(indexes[indexes.len() - tail..].iter())
        .copied()
        .collect()
}

fn add_context_ranges(states: &mut [LineState], indexes: &[usize]) {
    for &index in indexes {
        let start = index.saturating_sub(FOCUS_CONTEXT_LINES);
        let end = (index + FOCUS_CONTEXT_LINES + 1).min(states.len());
        for state in &mut states[start..end] {
            state.selected = true;
        }
    }
}

fn add_status_lines(states: &mut [LineState]) {
    let status = states
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(index, state)| {
            (state.classification.status && !state.selected).then_some(index)
        })
        .take(MAX_STATUS_MATCHES)
        .collect::<Vec<_>>();
    for index in status {
        states[index].selected = true;
    }
}

fn ordered_line_indexes(states: &[LineState]) -> impl Iterator<Item = usize> + '_ {
    states
        .iter()
        .enumerate()
        .filter_map(|(index, state)| state.selected.then_some(index))
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
    fn summary_threshold_probe_preserves_lines_at_byte_and_line_boundaries() {
        let at_threshold = "one\ntwo\nthree";
        assert_eq!(collect_lines_for_summary(at_threshold, 128, 3), None);

        let above_threshold = "one\ntwo\nthree\nfour";
        assert_eq!(
            collect_lines_for_summary(above_threshold, 128, 3),
            Some(vec!["one", "two", "three", "four"])
        );
        assert_eq!(
            collect_lines_for_summary("abcdef", 5, 20),
            Some(vec!["abcdef"])
        );
        let large_threshold = DEFAULT_SUMMARY_AFTER_LINES + 100;
        let output = "line\n".repeat(large_threshold + 1);
        assert_eq!(
            collect_lines_for_summary(&output, usize::MAX, large_threshold),
            Some(vec!["line"; large_threshold + 1])
        );
        assert_eq!(
            collect_lines_for_summary(&output, usize::MAX, usize::MAX),
            None
        );
    }

    #[test]
    fn line_classification_reuses_normalization_without_retaining_previous_signals() {
        let mut lowercase = String::new();
        let classification = classify_line("  ERROR: warning; tests PASSED", &mut lowercase);
        assert!(classification.critical);
        assert!(classification.advisory);
        assert!(classification.status);
        let classification = classify_line("ordinary output", &mut lowercase);
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
        let states = select_line_states(&lines, true, true);
        let ordered = ordered_line_indexes(&states).collect::<Vec<_>>();

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
