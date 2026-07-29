use super::*;
use codex_protocol::exec_output::StreamOutput;
use codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS;
use codex_utils_output_truncation::TruncationReason;
use codex_utils_output_truncation::approx_token_count;

#[test]
fn shell_projection_uses_shared_success_default_and_returns_metadata() {
    let body = "x".repeat(20_000);
    let output = ExecToolCallOutput {
        aggregated_output: StreamOutput::new(body.clone()),
        ..ExecToolCallOutput::default()
    };

    let projected = project_exec_output_text_with_budget(
        &output,
        TruncationPolicy::Tokens(20_000),
        /*requested_limit*/ None,
        Some("echo ok"),
    );
    assert_eq!(
        projected.truncation_metadata,
        TruncationMetadata {
            requested_limit: None,
            default_limit: DEFAULT_SUCCESS_OUTPUT_TOKENS,
            hard_limit: 20_000,
            applied_limit: DEFAULT_SUCCESS_OUTPUT_TOKENS,
            original_size: approx_token_count(&body),
            retained_size: DEFAULT_SUCCESS_OUTPUT_TOKENS,
            truncation_reason: TruncationReason::DefaultLimit,
        }
    );
    assert!(projected.text.contains("Warning: truncated output"));
}
