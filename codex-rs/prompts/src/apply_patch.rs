/// Authoring instructions included in the model-visible `apply_patch` tool description.
pub const APPLY_PATCH_TOOL_INSTRUCTIONS: &str =
    include_str!("../templates/apply_patch_tool_instructions.md");

/// The common forms, repeated in every request's eager `exec` description. The complete
/// contract stays one `resolve_tool("apply_patch")` away.
pub const APPLY_PATCH_EAGER_INSTRUCTIONS: &str =
    include_str!("../templates/apply_patch_eager_instructions.md");
