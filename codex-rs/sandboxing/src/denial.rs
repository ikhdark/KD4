use codex_protocol::exec_output::ExecToolCallOutput;

use crate::SandboxType;

/// Returns whether a failed command was likely denied by the selected sandbox.
pub fn is_likely_sandbox_denied(
    sandbox_type: SandboxType,
    exec_output: &ExecToolCallOutput,
) -> bool {
    if sandbox_type == SandboxType::None || exec_output.exit_code == 0 {
        return false;
    }

    // Limit fallback text detection to diagnostics emitted by the operating
    // system. Application prose that merely mentions a sandbox or a failed
    // write is not evidence that the sandbox denied the process.
    const SANDBOX_DENIED_KEYWORDS: [&str; 3] = [
        "permission denied",
        "operation not permitted",
        "read-only file system",
    ];

    let has_sandbox_keyword = [
        &exec_output.stderr.text,
        &exec_output.stdout.text,
        &exec_output.aggregated_output.text,
    ]
    .into_iter()
    .any(|section| {
        let lower = section.to_lowercase();
        SANDBOX_DENIED_KEYWORDS
            .iter()
            .any(|needle| lower.contains(needle))
    });

    if has_sandbox_keyword {
        return true;
    }

    const QUICK_REJECT_EXIT_CODES: [i32; 3] = [2, 126, 127];
    if QUICK_REJECT_EXIT_CODES.contains(&exec_output.exit_code) {
        return false;
    }

    false
}
