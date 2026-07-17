use std::collections::BTreeSet;

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

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct ShellOutputReduction {
    pub(crate) summary: String,
    pub(crate) omitted_bytes: usize,
    pub(crate) omitted_lines: usize,
}

pub(crate) fn summarize_shell_output_for_model(
    output: &str,
    exit_code: i32,
    timed_out: bool,
    options: ShellOutputSummaryOptions<'_>,
) -> Option<String> {
    reduce_shell_output_for_model(output, exit_code, timed_out, options)
        .map(|reduction| reduction.summary)
}

pub(crate) fn reduce_shell_output_for_model(
    output: &str,
    exit_code: i32,
    timed_out: bool,
    options: ShellOutputSummaryOptions<'_>,
) -> Option<ShellOutputReduction> {
    reduce_shell_output_for_model_inner(
        output,
        exit_code,
        timed_out,
        options,
        SUMMARY_MAX_BYTES,
        /*force_over_budget*/ false,
    )
}

pub(crate) fn reduce_shell_output_for_model_with_budget(
    output: &str,
    exit_code: i32,
    timed_out: bool,
    options: ShellOutputSummaryOptions<'_>,
    max_bytes: usize,
) -> Option<ShellOutputReduction> {
    reduce_shell_output_for_model_inner(
        output,
        exit_code,
        timed_out,
        options,
        max_bytes.min(SUMMARY_MAX_BYTES),
        /*force_over_budget*/ true,
    )
}

fn reduce_shell_output_for_model_inner(
    output: &str,
    exit_code: i32,
    timed_out: bool,
    options: ShellOutputSummaryOptions<'_>,
    summary_max_bytes: usize,
    force_over_budget: bool,
) -> Option<ShellOutputReduction> {
    if !options.enabled {
        return None;
    }

    let line_count = output.lines().count();
    let (byte_threshold, line_threshold) = if options.turn_cost_guard {
        (EARLY_SUMMARY_AFTER_BYTES, EARLY_SUMMARY_AFTER_LINES)
    } else {
        (DEFAULT_SUMMARY_AFTER_BYTES, DEFAULT_SUMMARY_AFTER_LINES)
    };
    if output.len() <= byte_threshold
        && line_count <= line_threshold
        && (!force_over_budget || output.len() <= summary_max_bytes)
    {
        return None;
    }

    let lines = output.lines().collect::<Vec<_>>();
    let failed = timed_out || exit_code != 0;
    let validation = options
        .command_text
        .is_some_and(looks_like_validation_command);
    let selected = selected_line_indexes(&lines, failed, validation);
    let source_lines = output.split_inclusive('\n').collect::<Vec<_>>();
    let retained_shape = if validation {
        "failure-focused lines, final status lines, tail"
    } else if failed {
        "failure-focused lines, tail"
    } else {
        "head, warning/error lines, tail"
    };

    let largest_header = render_summary_header(
        line_count,
        output.len(),
        exit_code,
        timed_out,
        retained_shape,
        usize::MAX,
        usize::MAX,
    );
    let header_lines = largest_header.lines().count().saturating_add(1);
    let body_bytes = summary_max_bytes.saturating_sub(largest_header.len().saturating_add(2));
    let body_lines = SUMMARY_MAX_LINES.saturating_sub(header_lines);
    let mut builder = SummaryBuilder::with_limits(body_bytes, body_lines);
    builder.push_line("Selected output lines:");

    let mut previous = None;
    let mut retained_source_bytes = 0usize;
    let mut retained_source_lines = 0usize;
    let selected_count = selected.len();
    for (selected_position, index) in selected.into_iter().enumerate() {
        if let Some(previous_index) = previous
            && index > previous_index + 1
        {
            if !builder.push_line("...") {
                break;
            }
        }
        if let Some(line) = lines.get(index) {
            let source_span_len = source_lines
                .get(index)
                .map_or(line.len(), |span| span.len());
            let Some(retained_bytes) =
                builder.push_numbered_source_line(index + 1, line, source_span_len)
            else {
                break;
            };
            retained_source_bytes = retained_source_bytes.saturating_add(retained_bytes);
            retained_source_lines = retained_source_lines.saturating_add(1);
        }
        previous = Some(index);
        if builder.is_full() {
            builder.capped |= selected_position + 1 < selected_count;
            break;
        }
    }
    let omitted_source_bytes = output.len().saturating_sub(retained_source_bytes);
    let omitted_source_lines = line_count.saturating_sub(retained_source_lines);
    let header = render_summary_header(
        line_count,
        output.len(),
        exit_code,
        timed_out,
        retained_shape,
        omitted_source_lines,
        omitted_source_bytes,
    );
    match builder.finish() {
        Some(body) => Some(ShellOutputReduction {
            summary: format!("{header}\n\n{body}"),
            omitted_bytes: omitted_source_bytes,
            omitted_lines: omitted_source_lines,
        }),
        None if force_over_budget => Some(compact_output_reduction(output, summary_max_bytes)),
        None => None,
    }
}

fn compact_output_reduction(output: &str, cap: usize) -> ShellOutputReduction {
    let mut omitted_bytes = output.len();
    let mut omitted_lines = output
        .as_bytes()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    let mut marker = format!("\n[+{omitted_bytes}B/{omitted_lines}L]\n");
    let mut head = "";
    let mut tail = "";

    for _ in 0..8 {
        if marker.len() >= cap {
            break;
        }
        let data_budget = cap - marker.len();
        head = take_prefix_at_char_boundary(output, data_budget / 2);
        let remaining = &output[head.len()..];
        tail = take_suffix_at_char_boundary(remaining, data_budget - head.len());
        let omitted_end = output.len().saturating_sub(tail.len());
        let omitted = &output.as_bytes()[head.len().min(omitted_end)..omitted_end];
        let next_omitted_bytes = omitted.len();
        let next_omitted_lines = omitted.iter().filter(|byte| **byte == b'\n').count();
        let next_marker = format!("\n[+{next_omitted_bytes}B/{next_omitted_lines}L]\n");
        if next_marker.len() == marker.len()
            && next_omitted_bytes == omitted_bytes
            && next_omitted_lines == omitted_lines
        {
            marker = next_marker;
            omitted_bytes = next_omitted_bytes;
            omitted_lines = next_omitted_lines;
            break;
        }
        marker = next_marker;
        omitted_bytes = next_omitted_bytes;
        omitted_lines = next_omitted_lines;
    }

    let summary = if marker.len() >= cap {
        marker
    } else {
        format!("{head}{marker}{tail}")
    };
    ShellOutputReduction {
        summary,
        omitted_bytes,
        omitted_lines,
    }
}

fn render_summary_header(
    line_count: usize,
    byte_count: usize,
    exit_code: i32,
    timed_out: bool,
    retained_shape: &str,
    omitted_lines: usize,
    omitted_bytes: usize,
) -> String {
    let mut lines = vec![
        "Shell output summary:".to_string(),
        format!("- original_lines: {line_count}"),
        format!("- original_bytes: {byte_count}"),
        format!("- exit_code: {exit_code}"),
    ];
    if timed_out {
        lines.push("- timed_out: true".to_string());
    }
    lines.extend([
        format!("- retained: {retained_shape}"),
        format!("- omitted_lines: {omitted_lines}"),
        format!("- omitted_bytes: {omitted_bytes}"),
    ]);
    lines.join("\n")
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
    max_bytes: usize,
    max_lines: usize,
}

impl SummaryBuilder {
    fn with_limits(max_bytes: usize, max_lines: usize) -> Self {
        Self {
            text: String::new(),
            lines: 0,
            capped: false,
            max_bytes,
            max_lines,
        }
    }

    fn push_line(&mut self, line: impl AsRef<str>) -> bool {
        if self.is_full() {
            self.capped = true;
            return false;
        }

        let line = line.as_ref();
        if self.lines + 1 > self.max_lines {
            self.capped = true;
            return false;
        }

        let separator_bytes = usize::from(!self.text.is_empty());
        let remaining = self
            .max_bytes
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

        self.append_rendered_line(&rendered);
        true
    }

    fn push_numbered_source_line(
        &mut self,
        number: usize,
        source: &str,
        source_span_len: usize,
    ) -> Option<usize> {
        if self.is_full() || self.lines + 1 > self.max_lines {
            self.capped = true;
            return None;
        }
        let separator_bytes = usize::from(!self.text.is_empty());
        let remaining = self
            .max_bytes
            .saturating_sub(self.text.len())
            .saturating_sub(separator_bytes);
        let prefix = format!("{number:>5}: ");
        if remaining <= prefix.len() {
            self.capped = true;
            return None;
        }
        let source_budget = remaining.saturating_sub(prefix.len());
        let (rendered_source, retained_source_bytes) =
            summarize_oversized_source(source, source_budget);
        let retained_source_bytes = if retained_source_bytes == source.len() {
            source_span_len
        } else {
            retained_source_bytes
        };
        self.capped |= retained_source_bytes < source_span_len;
        self.append_rendered_line(&format!("{prefix}{rendered_source}"));
        Some(retained_source_bytes)
    }

    fn append_rendered_line(&mut self, rendered: &str) {
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(rendered);
        self.lines = self.lines.saturating_add(1);
    }

    fn is_full(&self) -> bool {
        self.lines >= self.max_lines || self.text.len() >= self.max_bytes
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
    summarize_oversized_source(line, max_bytes).0
}

fn summarize_oversized_source(line: &str, max_bytes: usize) -> (String, usize) {
    const MARKER: &str = " ... [line truncated] ... ";
    if line.len() <= max_bytes {
        return (line.to_string(), line.len());
    }
    if max_bytes <= MARKER.len() {
        let prefix = take_prefix_at_char_boundary(line, max_bytes);
        return (prefix.to_string(), prefix.len());
    }

    let payload_bytes = max_bytes - MARKER.len();
    let head_bytes = payload_bytes / 2;
    let tail_bytes = payload_bytes - head_bytes;
    let head = take_prefix_at_char_boundary(line, head_bytes);
    let tail = take_suffix_at_char_boundary(line, tail_bytes);
    (
        format!("{head}{MARKER}{tail}"),
        head.len().saturating_add(tail.len()),
    )
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
