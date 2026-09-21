//! Helpers for truncating tool and exec output using [`TruncationPolicy`](codex_protocol::protocol::TruncationPolicy).

use codex_protocol::models::FunctionCallOutputContentItem;
pub use codex_utils_string::approx_bytes_for_tokens;
pub use codex_utils_string::approx_token_count;
pub use codex_utils_string::approx_token_count_exceeds;
pub use codex_utils_string::approx_tokens_from_byte_count;
use std::borrow::Cow;
use std::fmt::Write;

pub use codex_protocol::protocol::TruncationPolicy;

/// Conservative coherent-packet model-projection defaults. Formation happens before token
/// minimization, and explicit requests remain bounded by the caller-supplied
/// model hard limit.
///
/// Successful output carries the evidence a turn is built on. Repository
/// discovery routinely produces several hundred lines of it, so the success
/// budget matches the failure budget rather than sitting below it; a stack
/// trace is not worth more room than the search results that answer the task.
pub const DEFAULT_SUCCESS_OUTPUT_TOKENS: usize = 10_000;
pub const DEFAULT_FAILURE_OUTPUT_TOKENS: usize = 10_000;
/// High-signal diagnostics stay above the ordinary budget: a compiler or test
/// dump is the one class whose useful part is reliably larger than a packet.
pub const DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS: usize = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputOutcome {
    Success,
    Failure,
    TimedOut,
    Skipped,
}

/// Classification supplied by the producing tool before model projection.
///
/// A shared projector can choose a ceiling without inspecting rendered output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputDiagnosticClass {
    Normal,
    HighSignal,
}

impl OutputOutcome {
    pub fn from_exit_status(exit_code: Option<i32>, timed_out: bool) -> Self {
        if timed_out {
            Self::TimedOut
        } else if exit_code == Some(0) {
            Self::Success
        } else {
            Self::Failure
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputLimitResolution {
    pub requested_limit: Option<usize>,
    pub default_limit: usize,
    pub hard_limit: usize,
    pub applied_limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedTextOutput {
    pub text: String,
    pub was_truncated: bool,
}

pub fn adaptive_output_budget_description() -> String {
    format!(
        "Defaults to a coherent packet of up to {DEFAULT_SUCCESS_OUTPUT_TOKENS} tokens; \
         failure/timeout and high-signal diagnostics use up to \
         {DEFAULT_FAILURE_OUTPUT_TOKENS} and {DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS} tokens respectively"
    )
}

pub fn resolve_output_limits(
    requested_limit: Option<usize>,
    outcome: OutputOutcome,
    command_text: Option<&str>,
    output_text: &str,
    hard_limit: usize,
) -> OutputLimitResolution {
    resolve_projected_output_limits(
        requested_limit,
        outcome,
        classify_diagnostic(command_text, output_text),
        hard_limit,
    )
}

/// Resolves a model-visible projection using producer-supplied metadata.
///
/// This intentionally does not inspect output text. Text classification, if a
/// producer needs it, belongs at the producer boundary before rendering.
pub fn resolve_projected_output_limits(
    requested_limit: Option<usize>,
    outcome: OutputOutcome,
    diagnostic_class: OutputDiagnosticClass,
    hard_limit: usize,
) -> OutputLimitResolution {
    let default_limit = match diagnostic_class {
        OutputDiagnosticClass::HighSignal => DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS,
        OutputDiagnosticClass::Normal => match outcome {
            OutputOutcome::Success | OutputOutcome::Skipped => DEFAULT_SUCCESS_OUTPUT_TOKENS,
            OutputOutcome::Failure | OutputOutcome::TimedOut => DEFAULT_FAILURE_OUTPUT_TOKENS,
        },
    };

    OutputLimitResolution {
        requested_limit,
        default_limit,
        hard_limit,
        applied_limit: requested_limit.unwrap_or(default_limit).min(hard_limit),
    }
}

/// Classifies raw producer output for tools that do not have a stronger native
/// diagnostic signal. Model projection consumes the resulting enum rather than
/// parsing a rendered response.
pub fn classify_diagnostic(command_text: Option<&str>, output_text: &str) -> OutputDiagnosticClass {
    if is_high_signal_diagnostic(command_text, output_text) {
        OutputDiagnosticClass::HighSignal
    } else {
        OutputDiagnosticClass::Normal
    }
}

pub fn truncate_text_with_output_limit(
    content: &str,
    limits: OutputLimitResolution,
) -> TruncatedTextOutput {
    let text = truncate_text_to_token_ceiling_cow(content, limits.applied_limit);
    let was_truncated = matches!(text, Cow::Owned(_));
    TruncatedTextOutput {
        text: text.into_owned(),
        was_truncated,
    }
}

/// Truncates text while ensuring the returned value itself does not exceed the
/// requested approximate-token ceiling, including any truncation marker.
pub fn truncate_text_to_token_ceiling(content: &str, max_tokens: usize) -> String {
    truncate_text_to_token_ceiling_cow(content, max_tokens).into_owned()
}

fn truncate_text_to_token_ceiling_cow(content: &str, max_tokens: usize) -> Cow<'_, str> {
    if !approx_token_count_exceeds(content, max_tokens) {
        return Cow::Borrowed(content);
    }
    if max_tokens == 0 {
        return Cow::Owned(String::new());
    }
    Cow::Owned(truncate_over_budget_text(content, max_tokens))
}

fn truncate_over_budget_text(content: &str, max_tokens: usize) -> String {
    const BEFORE_MIDDLE: &str = "\n[omitted before retained middle]\n";
    const AFTER_MIDDLE: &str = "\n[omitted after retained middle]\n";
    let marker_tokens = approx_token_count(BEFORE_MIDDLE) + approx_token_count(AFTER_MIDDLE);
    // Leave room for useful UTF-8 content in each retained region.
    if max_tokens <= marker_tokens + 4 {
        return truncate_middle_to_token_ceiling(content, max_tokens);
    }

    let mut retained_bytes =
        approx_bytes_for_tokens(max_tokens - marker_tokens).min(content.len().saturating_sub(1));
    let mut candidate = String::new();
    loop {
        let head = retained_bytes.saturating_mul(2) / 5;
        let middle = retained_bytes / 5;
        let tail = retained_bytes - head - middle;
        let middle_start = (content.len() - middle) / 2;
        candidate.clear();
        let _ = write!(
            candidate,
            "{}{BEFORE_MIDDLE}{}{AFTER_MIDDLE}{}",
            retained_text(content, 0, content.floor_char_boundary(head)),
            retained_text(
                content,
                content.ceil_char_boundary(middle_start),
                content.floor_char_boundary(middle_start + middle)
            ),
            retained_text(
                content,
                content.ceil_char_boundary(content.len() - tail),
                content.len()
            )
        );
        let actual_tokens = approx_token_count(&candidate);
        if actual_tokens <= max_tokens {
            return candidate;
        }
        // Scale by the observed token density; a token delta is not a byte delta.
        retained_bytes = retained_bytes
            .saturating_mul(max_tokens - marker_tokens)
            .checked_div(actual_tokens - marker_tokens)
            .unwrap_or(0)
            // Guarantee geometric progress even when the sampled density
            // changes across UTF-8 boundaries or retained regions.
            .min(retained_bytes.saturating_sub(retained_bytes.div_ceil(8)));
        if retained_bytes < 3 {
            return truncate_middle_to_token_ceiling(content, max_tokens);
        }
    }
}

// Prefer complete source lines at omission seams. If a sampled region cannot
// fit even one line, retain its UTF-8-safe fragment so a long line remains useful.
fn retained_text(content: &str, start: usize, end: usize) -> &str {
    // A sub-character sample can round its start past its end.
    let start = start.min(end);
    let region = &content[start..end];
    let line_start = if start == 0 || content.as_bytes()[start - 1] == b'\n' {
        0
    } else {
        region.find('\n').map_or(region.len(), |index| index + 1)
    };
    let line_end = if end == content.len() || region.ends_with('\n') {
        region.len()
    } else {
        region.rfind('\n').map_or(0, |index| index + 1)
    };
    if line_start < line_end {
        &region[line_start..line_end]
    } else {
        region
    }
}

fn truncate_middle_to_token_ceiling(content: &str, max_tokens: usize) -> String {
    const MARKER: &str = "\n[...]\n";
    let marker = if max_tokens >= approx_token_count(MARKER) + 4 {
        MARKER
    } else if max_tokens > 0 {
        "…"
    } else {
        ""
    };
    let mut bytes =
        approx_bytes_for_tokens(max_tokens - approx_token_count(marker)).min(content.len());
    let marker_tokens = approx_token_count(marker);
    let mut candidate = String::new();
    loop {
        let mut head = if marker.is_empty() {
            bytes
        } else {
            bytes.div_ceil(2)
        };
        if content.floor_char_boundary(head) == 0 {
            head = content.floor_char_boundary(bytes);
        }
        let tail = bytes - head;
        candidate.clear();
        let _ = write!(
            candidate,
            "{}{marker}{}",
            retained_text(content, 0, content.floor_char_boundary(head)),
            retained_text(
                content,
                content.ceil_char_boundary(content.len() - tail),
                content.len()
            )
        );
        let tokens = approx_token_count(&candidate);
        if tokens <= max_tokens {
            return candidate;
        }
        bytes = bytes
            .saturating_mul(max_tokens - marker_tokens)
            .checked_div(tokens - marker_tokens)
            .unwrap_or(0)
            .min(bytes.saturating_sub(bytes.div_ceil(8)));
    }
}

pub fn formatted_truncate_text_with_output_limit(
    content: &str,
    limits: OutputLimitResolution,
) -> TruncatedTextOutput {
    formatted_truncate_text_to_token_ceiling(content, limits.applied_limit)
}

fn formatted_truncate_text_to_token_ceiling(
    content: &str,
    max_tokens: usize,
) -> TruncatedTextOutput {
    let original_tokens = approx_token_count(content);
    if original_tokens <= max_tokens {
        return TruncatedTextOutput {
            text: content.to_owned(),
            was_truncated: false,
        };
    }
    let warning = format!(
        "Warning: truncated output (original token count: {original_tokens})\nTotal output lines: {}\n\n",
        content.lines().count(),
    );
    let warning_tokens = approx_token_count(&warning);
    // Keep at least half the budget for source evidence. At small budgets the
    // inline omission marker communicates truncation without consuming the
    // space needed to identify each retained text run.
    let text = if max_tokens > warning_tokens.saturating_mul(2) {
        format!(
            "{warning}{}",
            truncate_over_budget_text(content, max_tokens - warning_tokens)
        )
    } else {
        truncate_over_budget_text(content, max_tokens)
    };
    TruncatedTextOutput {
        text,
        was_truncated: true,
    }
}

/// Recognize validation output for diagnostic budgeting and summarization.
/// This heuristic does not grant execution permission or classify side effects.
pub fn looks_like_validation_command(command: &str) -> bool {
    let command = if command.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(command.to_ascii_lowercase())
    } else {
        Cow::Borrowed(command)
    };
    let words = command.split_ascii_whitespace().collect::<Vec<_>>();
    validation_invocation(&words)
}

fn validation_invocation(mut words: &[&str]) -> bool {
    loop {
        let Some((program, args)) = words.split_first() else {
            return false;
        };
        let program = program.trim_matches(['\'', '"']);
        let program = program.rsplit(['/', '\\']).next().unwrap_or(program);
        let program = program.strip_suffix(".exe").unwrap_or(program);
        // Only inspect executable positions and known launchers. Mentions in echo,
        // file reads, commit messages, or other arguments are not validation runs.
        match (program, args) {
            ("&" | "npx" | "bunx", args)
            | ("uv" | "poetry" | "pipx", ["run", args @ ..])
            | ("pnpm", ["exec", args @ ..]) => {
                words = args;
                continue;
            }
            _ => {}
        }
        if args
            .iter()
            .take_while(|arg| **arg != "--")
            .any(|arg| matches!(*arg, "--help" | "-h" | "--version"))
        {
            return false;
        }
        return match (program, args) {
            ("cargo", args) => {
                let args = if args.first().is_some_and(|arg| arg.starts_with('+')) {
                    &args[1..]
                } else {
                    args
                };
                matches!(
                    args,
                    ["build" | "check" | "test" | "nextest" | "clippy", ..]
                )
            }
            ("rustc" | "pytest" | "tsc" | "eslint" | "ruff" | "mypy", _) => true,
            ("python" | "python3" | "py", ["-m", "unittest" | "pytest", ..]) => true,
            ("python" | "python3" | "py", [script, ..]) => {
                script.rsplit(['/', '\\']).next() == Some("rust_test_runner.py")
            }
            ("just", args) => just_validation_invocation(args),
            ("npm" | "pnpm" | "yarn", ["test" | "build" | "lint" | "typecheck", ..])
            | ("npm" | "pnpm" | "yarn", ["run", "test" | "build" | "lint" | "typecheck", ..])
            | ("dotnet" | "go", ["test", ..])
            | ("node", ["--test", ..]) => true,
            _ => false,
        };
    }
}

fn just_validation_invocation(mut args: &[&str]) -> bool {
    while let Some((arg, rest)) = args.split_first() {
        match *arg {
            "--" => {
                args = rest;
                break;
            }
            "-f" | "--justfile" | "-d" | "--working-directory" => {
                let Some((value, remaining)) = rest.split_first() else {
                    return false;
                };
                args = remaining;
                // The command display can quote a path containing spaces.
                if let Some(quote @ ('\'' | '"')) = value.chars().next()
                    && !value.ends_with(quote)
                {
                    let Some(end) = args.iter().position(|word| word.ends_with(quote)) else {
                        return false;
                    };
                    args = &args[end + 1..];
                }
            }
            "-q" | "--quiet" | "-v" | "--verbose" | "--no-dotenv" => args = rest,
            value
                if value.starts_with("--justfile=")
                    || value.starts_with("--working-directory=") =>
            {
                args = rest;
            }
            _ => break,
        }
    }
    matches!(
        args,
        [
            "test"
                | "test-fast"
                | "core-test"
                | "core-test-fast"
                | "core-gate"
                | "core-test-lane"
                | "_core-test-lane-reserved"
                | "core-test-parity"
                | "test-lane"
                | "test-lane-main"
                | "test-lane-fast"
                | "test-lane-package"
                | "validate-crate"
                | "validate-crate-focused"
                | "validate-crate-full"
                | "clippy"
                | "clippy-workspace"
                | "clippy-lane"
                | "fmt-check"
                | "fmt-check-fast"
                | "sdk-ts-check"
                | "sdk-python-check"
                | "config-schema-check"
                | "config-schema-protocol-check"
                | "app-server-schema-check"
                | "source-map-check"
                | "source-owners-check"
                | "check"
                | "fix",
            ..,
        ]
    )
}

fn is_high_signal_diagnostic(command_text: Option<&str>, output_text: &str) -> bool {
    if command_text.is_some_and(looks_like_validation_command) {
        return true;
    }

    let mut lines = output_text.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.trim_start();
        let failure_list = line.eq_ignore_ascii_case("failures:") && {
            while lines.peek().is_some_and(|next| next.trim().is_empty()) {
                lines.next();
            }
            lines
                .peek()
                .is_some_and(|next| next.len() > next.trim_start().len())
        };
        let compiler_error = line
            .split_inclusive(':')
            .filter_map(|part| part.strip_suffix(':'))
            .any(|part| {
                let part = part.trim_start();
                part.eq_ignore_ascii_case("error")
                    || part.eq_ignore_ascii_case("warning")
                    || ["error ts", "error msb"].iter().any(|prefix| {
                        strip_prefix_ascii_case(part, prefix).is_some_and(|code| {
                            !code.is_empty() && code.bytes().all(|byte| byte.is_ascii_digit())
                        })
                    })
            });
        let rust_panic = strip_prefix_ascii_case(line, "thread '").is_some_and(|rest| {
            rest.split_once('\'').is_some_and(|(name, message)| {
                !name.is_empty() && strip_prefix_ascii_case(message, " panicked at").is_some()
            })
        });
        if compiler_error
            || rust_panic
            || line.eq_ignore_ascii_case("FAILED")
            || failure_list
            || line.eq_ignore_ascii_case("caused by:")
            || [
                "stack backtrace:",
                "traceback (most recent call last):",
                "error[e",
                "npm err!",
                "test result: failed",
                "assertionerror",
                "segmentation fault",
            ]
            .iter()
            .any(|prefix| strip_prefix_ascii_case(line, prefix).is_some())
        {
            return true;
        }
    }
    false
}

fn strip_prefix_ascii_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|start| start.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

pub fn formatted_truncate_text(content: &str, policy: TruncationPolicy) -> String {
    if let TruncationPolicy::Tokens(max_tokens) = policy {
        return formatted_truncate_text_to_token_ceiling(content, max_tokens).text;
    }
    if content.len() <= policy.byte_budget() {
        return content.to_string();
    }

    let original_token_count = approx_token_count(content);
    let total_lines = content.lines().count();
    let result = truncate_text(content, policy);
    format!(
        "Warning: truncated output (original token count: {original_token_count})\nTotal output lines: {total_lines}\n\n{result}"
    )
}

pub fn truncate_text(content: &str, policy: TruncationPolicy) -> String {
    policy.truncate_text(content)
}

/// Formats truncation warnings and shares the budget across contiguous text
/// runs, preserving their positions relative to images and encrypted content.
/// Unlike `truncate_function_output_items_with_policy`, this retains a share
/// of each text run instead of spending the budget on the earliest items.
pub fn formatted_truncate_text_content_items_with_policy(
    items: &[FunctionCallOutputContentItem],
    policy: TruncationPolicy,
) -> (Vec<FunctionCallOutputContentItem>, Option<usize>) {
    let mut text_segments = items.iter().filter_map(|item| match item {
        FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
        FunctionCallOutputContentItem::InputImage { .. }
        | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
    });

    let Some(first) = text_segments.next() else {
        return (items.to_vec(), None);
    };

    let mut combined = Cow::Borrowed(first);
    for text in text_segments {
        if !combined.is_empty() {
            combined.to_mut().push('\n');
        }
        combined.to_mut().push_str(text);
    }

    let mut original_token_count = None;
    let within_budget = match policy {
        TruncationPolicy::Bytes(max_bytes) => combined.len() <= max_bytes,
        TruncationPolicy::Tokens(max_tokens) => {
            let count = approx_token_count(&combined);
            original_token_count = Some(count);
            count <= max_tokens
        }
    };
    if within_budget {
        return (items.to_vec(), None);
    }

    let original_token_count =
        original_token_count.unwrap_or_else(|| approx_token_count(&combined));
    let runs = items
        .chunk_by(|left, right| {
            matches!(
                (left, right),
                (
                    FunctionCallOutputContentItem::InputText { .. },
                    FunctionCallOutputContentItem::InputText { .. }
                )
            )
        })
        .map(|run| {
            let mut text = Cow::Borrowed("");
            for item in run {
                if let FunctionCallOutputContentItem::InputText { text: part } = item {
                    if text.is_empty() {
                        text = Cow::Borrowed(part.as_str());
                    } else {
                        text.to_mut().push('\n');
                        text.to_mut().push_str(part);
                    }
                }
            }
            let cost = match policy {
                TruncationPolicy::Bytes(_) => text.len(),
                TruncationPolicy::Tokens(_) => approx_token_count(&text),
            };
            (run, text, cost)
        })
        .collect::<Vec<_>>();
    let mut remaining_cost = runs.iter().map(|(_, _, cost)| cost).sum::<usize>();
    let mut remaining_budget = match policy {
        TruncationPolicy::Bytes(limit) | TruncationPolicy::Tokens(limit) => limit,
    };
    let mut out = Vec::with_capacity(items.len());
    for (run, text, cost) in runs {
        if !matches!(run[0], FunctionCallOutputContentItem::InputText { .. }) {
            out.extend_from_slice(run);
            continue;
        }
        let budget = if remaining_cost == 0 {
            0
        } else {
            ((remaining_budget as u128 * cost as u128) / remaining_cost as u128) as usize
        };
        remaining_budget -= budget;
        remaining_cost -= cost;
        let run_policy = match policy {
            TruncationPolicy::Bytes(_) => TruncationPolicy::Bytes(budget),
            TruncationPolicy::Tokens(_) => TruncationPolicy::Tokens(budget),
        };
        out.push(FunctionCallOutputContentItem::InputText {
            text: formatted_truncate_text(&text, run_policy),
        });
    }

    (out, Some(original_token_count))
}

/// Spends the budget in source order and reports how many later text items
/// were omitted. Non-text items keep their original relative order.
pub fn truncate_function_output_items_with_policy(
    items: &[FunctionCallOutputContentItem],
    policy: TruncationPolicy,
) -> Vec<FunctionCallOutputContentItem> {
    let mut out: Vec<FunctionCallOutputContentItem> = Vec::with_capacity(items.len());
    let mut remaining_budget = match policy {
        TruncationPolicy::Bytes(_) => policy.byte_budget(),
        TruncationPolicy::Tokens(_) => policy.token_budget(),
    };
    let mut omitted_text_items = 0usize;

    for item in items {
        match item {
            FunctionCallOutputContentItem::InputText { text } => {
                if remaining_budget == 0 {
                    omitted_text_items += 1;
                    continue;
                }

                let cost = match policy {
                    TruncationPolicy::Bytes(_) => text.len(),
                    TruncationPolicy::Tokens(_) => approx_token_count(text),
                };

                if cost <= remaining_budget {
                    out.push(FunctionCallOutputContentItem::InputText { text: text.clone() });
                    remaining_budget = remaining_budget.saturating_sub(cost);
                } else {
                    let snippet_policy = match policy {
                        TruncationPolicy::Bytes(_) => TruncationPolicy::Bytes(remaining_budget),
                        TruncationPolicy::Tokens(_) => TruncationPolicy::Tokens(remaining_budget),
                    };
                    let snippet = truncate_text(text, snippet_policy);
                    if snippet.is_empty() {
                        omitted_text_items += 1;
                    } else {
                        out.push(FunctionCallOutputContentItem::InputText { text: snippet });
                    }
                    remaining_budget = 0;
                }
            }
            FunctionCallOutputContentItem::InputImage { image_url, detail } => {
                out.push(FunctionCallOutputContentItem::InputImage {
                    image_url: image_url.clone(),
                    detail: *detail,
                });
            }
            FunctionCallOutputContentItem::EncryptedContent { encrypted_content } => {
                out.push(FunctionCallOutputContentItem::EncryptedContent {
                    encrypted_content: encrypted_content.clone(),
                });
            }
        }
    }

    if omitted_text_items > 0 {
        out.push(FunctionCallOutputContentItem::InputText {
            text: format!("[omitted {omitted_text_items} text items ...]"),
        });
    }

    out
}

pub fn approx_tokens_from_byte_count_i64(bytes: i64) -> i64 {
    if bytes <= 0 {
        return 0;
    }

    let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
    i64::try_from(approx_tokens_from_byte_count(bytes)).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod truncate_tests;
