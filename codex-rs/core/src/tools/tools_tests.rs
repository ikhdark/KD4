use super::*;
use codex_protocol::exec_output::StreamOutput;

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
}
