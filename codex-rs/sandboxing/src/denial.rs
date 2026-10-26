use codex_protocol::exec_output::ExecToolCallOutput;

use crate::SandboxType;

/// Returns a best-effort permission-error hint from failed command output.
/// This cannot identify the enforcement layer and does not authorize an unsandboxed retry.
pub fn is_likely_sandbox_denied(
    sandbox_type: SandboxType,
    exec_output: &ExecToolCallOutput,
) -> bool {
    if sandbox_type == SandboxType::None || exec_output.exit_code == 0 {
        return false;
    }

    const SANDBOX_DENIED_KEYWORDS: [&str; 4] = [
        "permission denied",
        "access is denied",
        "operation not permitted",
        "read-only file system",
    ];

    [
        &exec_output.stderr.text,
        &exec_output.stdout.text,
        &exec_output.aggregated_output.text,
    ]
    .into_iter()
    .any(|section| {
        SANDBOX_DENIED_KEYWORDS.iter().any(|needle| {
            section
                .as_bytes()
                .windows(needle.len())
                .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
        })
    })
}

