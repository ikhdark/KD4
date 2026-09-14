//! Helpers for truncating tool and exec output using [`TruncationPolicy`](codex_protocol::protocol::TruncationPolicy).

use codex_protocol::models::FunctionCallOutputContentItem;
pub use codex_utils_string::approx_bytes_for_tokens;
pub use codex_utils_string::approx_token_count;
pub use codex_utils_string::approx_tokens_from_byte_count;

pub use codex_protocol::protocol::TruncationPolicy;

/// Conservative coherent-packet model-projection defaults. Formation happens before token
/// minimization, and explicit requests remain bounded by the caller-supplied
/// model hard limit.
pub const DEFAULT_SUCCESS_OUTPUT_TOKENS: usize = 4_000;
pub const DEFAULT_FAILURE_OUTPUT_TOKENS: usize = 10_000;
pub const DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS: usize = 10_000;

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
    let text = truncate_text_to_token_ceiling(content, limits.applied_limit);
    let was_truncated = text != content;
    TruncatedTextOutput {
        text,
        was_truncated,
    }
}

/// Truncates text while ensuring the returned value itself does not exceed the
/// requested approximate-token ceiling, including any truncation marker.
pub fn truncate_text_to_token_ceiling(content: &str, max_tokens: usize) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    if approx_token_count(content) <= max_tokens {
        return content.to_string();
    }

    const BEFORE_MIDDLE: &str = "\n[omitted before retained middle]\n";
    const AFTER_MIDDLE: &str = "\n[omitted after retained middle]\n";
    let marker_tokens = approx_token_count(BEFORE_MIDDLE) + approx_token_count(AFTER_MIDDLE);
    // Leave room for useful UTF-8 content in each retained region.
    if max_tokens <= marker_tokens + 4 {
        return truncate_middle_to_token_ceiling(content, max_tokens);
    }

    let mut retained_bytes =
        approx_bytes_for_tokens(max_tokens - marker_tokens).min(content.len().saturating_sub(1));
    loop {
        let head = retained_bytes.saturating_mul(2) / 5;
        let middle = retained_bytes / 5;
        let tail = retained_bytes - head - middle;
        let middle_start = (content.len() - middle) / 2;
        let candidate = format!(
            "{}{BEFORE_MIDDLE}{}{AFTER_MIDDLE}{}",
            &content[..content.floor_char_boundary(head)],
            &content[content.ceil_char_boundary(middle_start)
                ..content.ceil_char_boundary(middle_start + middle)],
            &content[content.ceil_char_boundary(content.len() - tail)..]
        );
        let actual_tokens = approx_token_count(&candidate);
        if actual_tokens <= max_tokens {
            return candidate;
        }
        retained_bytes = retained_bytes.saturating_sub(actual_tokens - max_tokens);
        if retained_bytes < 3 {
            return truncate_middle_to_token_ceiling(content, max_tokens);
        }
    }
}

fn truncate_middle_to_token_ceiling(content: &str, max_tokens: usize) -> String {
    const MARKER: &str = "\n[...]\n";
    let marker = if max_tokens >= approx_token_count(MARKER) + 4 {
        MARKER
    } else {
        ""
    };
    let mut bytes =
        approx_bytes_for_tokens(max_tokens - approx_token_count(marker)).min(content.len());
    loop {
        let head = if marker.is_empty() {
            bytes
        } else {
            bytes.div_ceil(2)
        };
        let tail = bytes - head;
        let candidate = format!(
            "{}{marker}{}",
            &content[..content.floor_char_boundary(head)],
            &content[content.ceil_char_boundary(content.len() - tail)..]
        );
        let tokens = approx_token_count(&candidate);
        if tokens <= max_tokens {
            return candidate;
        }
        bytes = bytes.saturating_sub(tokens - max_tokens);
    }
}

pub fn formatted_truncate_text_with_output_limit(
    content: &str,
    limits: OutputLimitResolution,
) -> TruncatedTextOutput {
    let original_tokens = approx_token_count(content);
    if original_tokens <= limits.applied_limit {
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
    let text = if limits.applied_limit > warning_tokens + 1 {
        format!(
            "{warning}{}",
            truncate_text_to_token_ceiling(content, limits.applied_limit - warning_tokens)
        )
    } else {
        truncate_text_to_token_ceiling(content, limits.applied_limit)
    };
    TruncatedTextOutput {
        text,
        was_truncated: true,
    }
}

fn is_high_signal_diagnostic(command_text: Option<&str>, output_text: &str) -> bool {
    let command = command_text.unwrap_or_default().to_ascii_lowercase();
    let diagnostic_command = [
        "cargo check",
        "cargo test",
        "cargo nextest",
        "cargo clippy",
        "rustc ",
        "pytest",
        "python -m unittest",
        "npm test",
        "npm run test",
        "pnpm test",
        "yarn test",
        "dotnet test",
        "go test",
        "just test",
        "just check",
    ]
    .iter()
    .any(|needle| command.contains(needle));
    if diagnostic_command {
        return true;
    }

    let output = output_text.to_ascii_lowercase();
    [
        "stack backtrace:",
        "traceback (most recent call last):",
        "thread 'main' panicked at",
        "error[e",
        "test result: failed",
        "failures:",
        "compiler error",
        "caused by:",
    ]
    .iter()
    .any(|needle| output.contains(needle))
}

pub fn formatted_truncate_text(content: &str, policy: TruncationPolicy) -> String {
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

pub fn formatted_truncate_text_content_items_with_policy(
    items: &[FunctionCallOutputContentItem],
    policy: TruncationPolicy,
) -> (Vec<FunctionCallOutputContentItem>, Option<usize>) {
    let text_segments = items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
            FunctionCallOutputContentItem::InputImage { .. }
            | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
        })
        .collect::<Vec<_>>();

    if text_segments.is_empty() {
        return (items.to_vec(), None);
    }

    let mut combined = String::new();
    for text in &text_segments {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(text);
    }

    if combined.len() <= policy.byte_budget() {
        return (items.to_vec(), None);
    }

    let original_token_count = approx_token_count(&combined);
    let mut out = vec![FunctionCallOutputContentItem::InputText {
        text: formatted_truncate_text(&combined, policy),
    }];
    out.extend(items.iter().filter_map(|item| match item {
        FunctionCallOutputContentItem::InputImage { image_url, detail } => {
            Some(FunctionCallOutputContentItem::InputImage {
                image_url: image_url.clone(),
                detail: *detail,
            })
        }
        FunctionCallOutputContentItem::EncryptedContent { encrypted_content } => {
            Some(FunctionCallOutputContentItem::EncryptedContent {
                encrypted_content: encrypted_content.clone(),
            })
        }
        FunctionCallOutputContentItem::InputText { .. } => None,
    }));

    (out, Some(original_token_count))
}

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
