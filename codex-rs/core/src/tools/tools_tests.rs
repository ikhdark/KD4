use super::*;
use codex_protocol::exec_output::StreamOutput;
use codex_utils_output_truncation::approx_token_count;

#[test]
fn shell_projection_uses_shared_success_default_and_reports_reduction() {
    let body = "x".repeat(20_000);
    let output = ExecToolCallOutput {
        aggregated_output: StreamOutput::new(body),
        ..ExecToolCallOutput::default()
    };

    let projected = project_exec_output_text_with_budget(
        &output,
        TruncationPolicy::Tokens(20_000),
        /*requested_limit*/ None,
        Some("echo ok"),
    );
    assert!(projected.reduced);
    assert!(projected.text.contains("Warning: truncated output"));
    assert!(
        approx_token_count(&projected.text)
            <= codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS
    );
}

#[test]
fn shell_projection_complete_envelope_respects_requested_limit() {
    let body = "{}[](),".repeat(10_000);
    let output = ExecToolCallOutput {
        aggregated_output: StreamOutput::new(body),
        ..ExecToolCallOutput::default()
    };

    let projected = project_exec_output_for_model_with_budget(
        &output,
        TruncationPolicy::Tokens(10_000),
        Some(64),
        Some("echo ok"),
    );

    assert!(projected.reduced);
    assert!(approx_token_count(&projected.text) <= 64);
}

#[test]
fn token_backfire_shell_projection_keeps_complete_output_that_fits_budget() {
    let body = (0..700)
        .map(|index| format!("line-{index}: exact evidence"))
        .collect::<Vec<_>>()
        .join("\n");
    let output = ExecToolCallOutput {
        aggregated_output: StreamOutput::new(body.clone()),
        ..ExecToolCallOutput::default()
    };

    let projected = project_exec_output_text_with_budget(
        &output,
        TruncationPolicy::Tokens(20_000),
        Some(20_000),
        Some("enumerate evidence"),
    );

    assert!(!projected.reduced);
    assert_eq!(projected.text, body);
}
