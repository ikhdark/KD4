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
        "Paths must be relative; never use absolute paths.",
        "Do not include unified-diff headers such as `diff --git`, `---`, or `+++`.",
        "After stale context, a concurrent edit, a context mismatch, or a failure that may have modified files, re-read only the affected current sections before retrying.",
        "For errors known to occur before file mutation, correct the error without re-reading unchanged contents.",
        "rename it without hunks",
        "( MoveTo { Hunk } | Hunk { Hunk } )",
        "single multiline argument, preserving actual line breaks",
    ] {
        assert!(
            APPLY_PATCH_TOOL_INSTRUCTIONS.contains(required),
            "apply_patch instructions lost required contract: {required}"
        );
    }
}
