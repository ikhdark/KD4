use super::APPLY_PATCH_TOOL_INSTRUCTIONS;

#[test]
fn apply_patch_instructions_stay_compact_and_complete() {
    assert!(
        APPLY_PATCH_TOOL_INSTRUCTIONS.len() <= 3_000,
        "apply_patch instructions grew to {} bytes",
        APPLY_PATCH_TOOL_INSTRUCTIONS.len()
    );

    for required in [
        "*** Begin Patch",
        "*** End Patch",
        "*** Add File:",
        "*** Delete File:",
        "*** Update File:",
        "*** Move to:",
        "@@",
        "Paths must be relative",
        "unified-diff headers",
        "re-read the relevant current section",
    ] {
        assert!(
            APPLY_PATCH_TOOL_INSTRUCTIONS.contains(required),
            "apply_patch instructions lost required contract: {required}"
        );
    }
}
