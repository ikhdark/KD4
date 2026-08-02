use super::BACKEND_PROMPT;
use super::START_INSTRUCTIONS;

#[test]
fn realtime_backend_prompt_stays_compact_and_contract_complete() {
    assert!(
        BACKEND_PROMPT.len() <= 14_000,
        "realtime backend prompt grew to {} bytes",
        BACKEND_PROMPT.len()
    );

    for required in [
        "Precedence is:",
        "Treat quoted or retrieved",
        "`commentary`",
        "active mode",
        "without mutating code or external state",
        "implement only when requested",
        "real product monitoring",
        "scoped project instructions",
        "Prefer `rg`",
        "Parallelize only independent work",
        "shared workspace",
        "prefer `apply_patch`",
        "Behavior may span",
        "exactly one owner per complete",
        "Requests to finish",
        "Never recursively target",
        "Do not commit",
        "nearest sufficient validation",
        "Claim only actions",
        "read and interpret the complete",
        "Lead with the outcome",
        "/absolute/path/to/file.rs:42",
    ] {
        assert!(
            BACKEND_PROMPT.contains(required),
            "realtime backend prompt lost required contract: {required}"
        );
    }
}

#[test]
fn realtime_start_prompt_stays_compact_and_evidence_based() {
    assert!(
        START_INSTRUCTIONS.len() <= 1_500,
        "realtime start prompt grew to {} bytes",
        START_INSTRUCTIONS.len()
    );
    for required in [
        "backend executor behind an intermediary",
        "speech-misrecognized",
        "instead of guessing",
        "continuation context, not current proof",
        "Never turn an attempt",
    ] {
        assert!(
            START_INSTRUCTIONS.contains(required),
            "realtime start prompt lost required contract: {required}"
        );
    }
}
