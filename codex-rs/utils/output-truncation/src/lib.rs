//! Helpers for truncating tool and exec output using [`TruncationPolicy`](codex_protocol::protocol::TruncationPolicy).

use codex_protocol::models::FunctionCallOutputContentItem;
pub use codex_utils_string::approx_bytes_for_tokens;
pub use codex_utils_string::approx_token_count;
pub use codex_utils_string::approx_tokens_from_byte_count;
use codex_utils_string::truncate_middle_chars;
use codex_utils_string::truncate_middle_with_token_budget;

pub use codex_protocol::protocol::TruncationPolicy;

pub const DEFAULT_SUCCESS_OUTPUT_TOKENS: usize = 4_000;
pub const DEFAULT_FAILURE_OUTPUT_TOKENS: usize = 8_000;
pub const DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputOutcome {
    Success,
    Failure,
    TimedOut,
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
        "Defaults adaptively to {DEFAULT_SUCCESS_OUTPUT_TOKENS} tokens for success, \
         {DEFAULT_FAILURE_OUTPUT_TOKENS} for failure/timeout, and up to \
         {DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS} for high-signal diagnostics"
    )
}

pub fn resolve_output_limits(
    requested_limit: Option<usize>,
    outcome: OutputOutcome,
    command_text: Option<&str>,
    output_text: &str,
    hard_limit: usize,
) -> OutputLimitResolution {
    let default_limit = if is_high_signal_diagnostic(command_text, output_text) {
        DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS
    } else {
        match outcome {
            OutputOutcome::Success => DEFAULT_SUCCESS_OUTPUT_TOKENS,
            OutputOutcome::Failure | OutputOutcome::TimedOut => DEFAULT_FAILURE_OUTPUT_TOKENS,
        }
    };

    OutputLimitResolution {
        requested_limit,
        default_limit,
        hard_limit,
        applied_limit: requested_limit.unwrap_or(default_limit).min(hard_limit),
    }
}

pub fn truncate_text_with_output_limit(
    content: &str,
    limits: OutputLimitResolution,
) -> TruncatedTextOutput {
    let text = truncate_text(content, TruncationPolicy::Tokens(limits.applied_limit));
    let was_truncated = text != content;
    TruncatedTextOutput {
        text,
        was_truncated,
    }
}

pub fn formatted_truncate_text_with_output_limit(
    content: &str,
    limits: OutputLimitResolution,
) -> TruncatedTextOutput {
    let mut truncated = truncate_text_with_output_limit(content, limits);
    if truncated.was_truncated {
        truncated.text = format!(
            "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{}",
            approx_token_count(content),
            content.lines().count(),
            truncated.text
        );
    }
    truncated
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
    match policy {
        TruncationPolicy::Bytes(bytes) => truncate_middle_chars(content, bytes),
        TruncationPolicy::Tokens(tokens) => truncate_middle_with_token_budget(content, tokens).0,
    }
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
