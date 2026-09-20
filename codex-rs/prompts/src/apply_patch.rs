/// Authoring instructions included in the model-visible `apply_patch` tool description.
pub const APPLY_PATCH_TOOL_INSTRUCTIONS: &str =
    include_str!("../templates/apply_patch_tool_instructions.md");

#[cfg(test)]
#[path = "apply_patch_contract_tests.rs"]
mod tests;
