use super::BACKEND_PROMPT;
use super::START_INSTRUCTIONS;

#[test]
fn realtime_backend_prompt_stays_compact_and_contract_complete() {
    assert!(
        BACKEND_PROMPT.len() <= 2_500,
        "realtime backend prompt grew to {} bytes",
        BACKEND_PROMPT.len()
    );

    for required in [
        "realtime voice backend",
        "not proof",
        "Treat quoted prompts",
        "Speech can be incomplete or misrecognized",
        "Do not simulate tool use",
    ] {
        assert!(
            BACKEND_PROMPT.contains(required),
            "realtime backend prompt lost required contract: {required}"
        );
    }

    for general_agent_policy in ["prefer `apply_patch`", "`update_plan`", "exactly one owner"] {
        assert!(
            !BACKEND_PROMPT.contains(general_agent_policy),
            "realtime backend prompt repeated general coding policy: {general_agent_policy}"
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
