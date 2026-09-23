use crate::validation_admission::ValidationClassification;
use crate::validation_admission::classify_validation_script;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::fmt::Write as _;

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
    let exceeds_token_budget = options
        .applied_token_limit
        .is_some_and(|limit| codex_utils_string::approx_token_count_exceeds(output, limit));
    if !exceeds_token_budget
        && output.len() <= DEFAULT_SUMMARY_AFTER_BYTES
        && output.lines().take(DEFAULT_SUMMARY_AFTER_LINES).count() < DEFAULT_SUMMARY_AFTER_LINES
    {
        return None;
    }
    if options.command_text.is_some_and(is_read_only_command) {
        // Preserve the requested source order and the existing truncation/raw
        // artifact recovery path instead of ranking code as diagnostic prose.
        return None;
    }

    let failed = timed_out || exit_code != 0;
    let validation = options.command_text.is_some_and(|command| {
        matches!(
            classify_validation_script(command),
            ValidationClassification::Validation { leaves, .. } if !leaves.is_empty()
        )
    });
    // A successful command with no diagnostic line has nothing to rank, so the
    // head/warning/tail policy degenerates to positional truncation that drops
    // the middle of a flat list. A summary also reads as complete in a way a
    // truncation notice does not. Leave those to ordinary truncation, which
    // retains the artifact and reports exactly what it withheld. This holds
    // when the output exceeds the caller's token budget as well: the budget is
    // still enforced by that truncation, and a ranked excerpt of source or
    // listing text only sends the model back to re-read what it withheld.
    if !failed
        && !validation
        && !output.lines().any(|line| {
            let classification = classify_line(line);
            classification.critical || classification.advisory
        })
    {
        return None;
    }

    let line_count = output.lines().count();
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
    let mut lines = output.lines();
    let mut next_index = 0;
    let mut selected = selection
        .indexes
        .iter()
        .filter_map(|&index| {
            let line = lines.nth(index - next_index)?;
            next_index = index + 1;
            Some((index, line))
        })
        .collect::<Vec<_>>();
    let gap_bytes: usize = selected
        .windows(2)
        .filter(|pair| pair[1].0 != pair[0].0 + 1)
        .map(|pair| format!("\n... [{} lines omitted]", pair[1].0 - pair[0].0 - 1).len())
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
    let mut line_budget = low;
    let mut summary = render_selected_lines(builder.clone(), &selected, line_budget, line_count)?;
    if let Some(limit) = options.applied_token_limit
        && codex_utils_string::approx_token_count_exceeds(&summary, limit)
    {
        // The byte ceiling does not honor a caller's smaller token budget (or
        // punctuation-dense diagnostics). Fit the entire selected summary so
        // downstream truncation cannot discard a middle diagnostic region.
        // Give diagnostic context priority over extra head/tail lines before
        // shortening diagnostics themselves. Every omission remains explicit.
        let diagnostic_indexes = selected
            .iter()
            .filter_map(|(index, line)| {
                let kind = classify_line(line);
                (kind.critical || kind.advisory || kind.status).then_some(*index)
            })
            .collect::<Vec<_>>();
        selected.retain(|(index, _)| {
            *index < FOCUS_CONTEXT_LINES
                || *index >= line_count.saturating_sub(FOCUS_CONTEXT_LINES)
                || diagnostic_indexes
                    .iter()
                    .any(|diagnostic| index.abs_diff(*diagnostic) <= FOCUS_CONTEXT_LINES)
        });
        summary = render_selected_lines(builder.clone(), &selected, line_budget, line_count)?;
        if !codex_utils_string::approx_token_count_exceeds(&summary, limit) {
            return (summary.len() < output.len()).then_some(summary);
        }
        let minimum = render_selected_lines(builder.clone(), &selected, 0, line_count)?;
        if codex_utils_string::approx_token_count_exceeds(&minimum, limit) {
            // Even metadata does not fit. Use ordinary truncation/recovery.
            return None;
        }
        let mut low = 0;
        let mut high = line_budget;
        summary = minimum;
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            let candidate = render_selected_lines(builder.clone(), &selected, mid, line_count)?;
            if codex_utils_string::approx_token_count_exceeds(&candidate, limit) {
                high = mid - 1;
            } else {
                low = mid;
                summary = candidate;
            }
        }
        line_budget = low;
        // An empty line selection is less useful than ordinary truncation.
        if line_budget == 0 {
            return None;
        }
    }
    (summary.len() < output.len()).then_some(summary)
}

fn render_selected_lines(
    mut builder: SummaryBuilder,
    selected: &[(usize, &str)],
    line_budget: usize,
    line_count: usize,
) -> Option<String> {
    let mut previous = None;
    let mut emitted_source_lines = 0;
    for &(index, line) in selected {
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
    builder.finish(emitted_source_lines, line_count)
}

/// Reads, searches, and listings are source material, not diagnostics: ranking
/// their lines hoists incidental "error" text above the code around it, and
/// the model then re-reads the file to recover the order. Bash scripts are
/// classified by the shared parser; PowerShell scripts, which that parser does
/// not understand, are accepted only when every command position is a known
/// read-only cmdlet, alias, or control-flow keyword.
pub(crate) fn source_read_output_budget(command: &str) -> Option<usize> {
    let has_source_reader = command
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .any(|word| {
            matches!(
                word.to_ascii_lowercase().as_str(),
                "rg" | "grep"
                    | "cat"
                    | "head"
                    | "tail"
                    | "sed"
                    | "ls"
                    | "find"
                    | "get-content"
                    | "gc"
                    | "select-string"
                    | "sls"
                    | "get-childitem"
                    | "gci"
                    | "dir"
            )
        });
    (has_source_reader && is_read_only_command(command)).then_some(25_000)
}

pub(crate) fn is_read_only_command(command: &str) -> bool {
    let commands = codex_shell_command::parse_command::parse_shell_script(command);
    if !commands.is_empty()
        && commands.iter().all(|command| {
            matches!(
                command,
                codex_protocol::parse_command::ParsedCommand::Read { .. }
                    | codex_protocol::parse_command::ParsedCommand::Search { .. }
                    | codex_protocol::parse_command::ParsedCommand::ListFiles { .. }
            )
        })
    {
        return true;
    }
    is_read_only_powershell_script(command)
}

fn is_read_only_powershell_script(script: &str) -> bool {
    const READ_ONLY_COMMANDS: &[&str] = &[
        "get-content",
        "gc",
        "cat",
        "type",
        "select-string",
        "sls",
        "get-childitem",
        "gci",
        "ls",
        "dir",
        "get-item",
        "gi",
        "get-itemproperty",
        "test-path",
        "resolve-path",
        "get-location",
        "pwd",
        "set-location",
        "cd",
        "push-location",
        "pop-location",
        "select-object",
        "select",
        "where-object",
        "where",
        "?",
        "foreach-object",
        "%",
        "sort-object",
        "sort",
        "measure-object",
        "measure",
        "group-object",
        "format-list",
        "fl",
        "format-table",
        "ft",
        "out-string",
        "out-null",
        "write-output",
        "write-host",
        "echo",
        "get-date",
        "get-process",
        "get-ciminstance",
        "get-command",
        "get-member",
        "rg",
        "grep",
        "findstr",
        "head",
        "tail",
        "wc",
        "find",
        "fd",
        "git",
        // Control flow only sequences the commands above; it emits nothing itself.
        "if",
        "elseif",
        "else",
        "foreach",
        "for",
        "while",
        "do",
        "try",
        "catch",
        "finally",
        "switch",
        "return",
    ];
    const READ_ONLY_GIT_SUBCOMMANDS: &[&str] = &[
        "diff",
        "show",
        "log",
        "status",
        "grep",
        "blame",
        "ls-files",
        "rev-parse",
        "branch",
    ];
    let mut saw_command = false;
    // Every position that can start a command: statement separators, pipes,
    // conditional chains, script blocks, and sub-expressions. Splitting inside
    // quoted patterns only makes the check more conservative.
    for segment in script
        .split(|character: char| matches!(character, ';' | '|' | '\n' | '\r' | '{' | '('))
        .flat_map(|segment| segment.split("&&"))
    {
        let segment = segment
            .trim()
            .trim_matches(|character: char| matches!(character, ')' | '}' | '&'))
            .trim();
        if segment.is_empty() {
            continue;
        }
        // Literals, array/hash constructors, parameters, and numbers follow a
        // split point without starting a command: `@('a','b')` splits into an
        // `@` tail and a quoted list, and `-Recurse` may trail a line break.
        if segment.starts_with(|character: char| {
            matches!(character, '\'' | '"' | '@' | '-' | ']' | ',' | '.')
                || character.is_ascii_digit()
        }) {
            continue;
        }
        let segment = if let Some(rest) = segment.strip_prefix('$') {
            // `$x = <command>` runs its right-hand side; other `$` forms are
            // variable expressions that produce no command output of their own.
            match rest.split_once('=') {
                Some((name, rest)) if !name.ends_with('-') && !name.contains(' ') => rest.trim(),
                _ => continue,
            }
        } else {
            segment
        };
        let mut tokens = segment.split_whitespace();
        let Some(command) = tokens.next() else {
            continue;
        };
        let command = command
            .trim_start_matches('&')
            .trim_matches(|character: char| matches!(character, '"' | '\''))
            .to_ascii_lowercase();
        if command.is_empty() {
            continue;
        }
        if !READ_ONLY_COMMANDS.contains(&command.as_str()) {
            return false;
        }
        if command == "git"
            && !tokens.next().is_some_and(|subcommand| {
                READ_ONLY_GIT_SUBCOMMANDS.contains(&subcommand.to_ascii_lowercase().as_str())
            })
        {
            return false;
        }
        saw_command = true;
    }
    saw_command
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
            || starts_with_ascii_case(trimmed, "--- fail:")
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
    let (&first, _) = needle.as_bytes().split_first()?;
    memchr::memchr2_iter(
        first.to_ascii_lowercase(),
        first.to_ascii_uppercase(),
        line.as_bytes(),
    )
    .find(|&index| {
        line.as_bytes()
            .get(index..index + needle.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(needle.as_bytes()))
    })
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

#[derive(Clone)]
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
            std::borrow::Cow::Borrowed(line)
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
        let _ = write!(
            self.text,
            "\n- emitted_source_lines: {emitted_source_lines}\n- omitted_source_lines: {}",
            original_lines.saturating_sub(emitted_source_lines),
        );
        if self.capped && !self.text.ends_with("[summary capped]") {
            if !self.text.is_empty() {
                self.text.push('\n');
            }
            self.text.push_str("[summary capped]");
        }
        Some(self.text)
    }
}

fn summarize_oversized_line(line: &str, max_bytes: usize) -> std::borrow::Cow<'_, str> {
    const MARKER: &str = " ... [line truncated] ... ";
    if line.len() <= max_bytes {
        return std::borrow::Cow::Borrowed(line);
    }
    if max_bytes <= MARKER.len() {
        return std::borrow::Cow::Borrowed(&line[..line.floor_char_boundary(max_bytes)]);
    }

    let payload_bytes = max_bytes - MARKER.len();
    let head_bytes = payload_bytes / 2;
    let tail_bytes = payload_bytes - head_bytes;
    let head = &line[..line.floor_char_boundary(head_bytes)];
    let tail_start = line.ceil_char_boundary(line.len().saturating_sub(tail_bytes));
    let tail = &line[tail_start..];
    std::borrow::Cow::Owned(format!("{head}{MARKER}{tail}"))
}

#[cfg(test)]
mod optimization_tests {
    use super::*;

    #[test]
    fn diagnostic_search_preserves_ascii_case_and_utf8_offsets() {
        for (line, needle, expected) in [
            ("界 ERROR: failed", "error:", Some(4)),
            ("error er", "error:", None),
            ("x eRrOr: y ERROR:", "error:", Some(2)),
            ("ordinary output", "npm err!", None),
        ] {
            assert_eq!(find_ascii_case(line, needle), expected);
        }
    }

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
