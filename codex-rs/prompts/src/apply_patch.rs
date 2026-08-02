/// Detailed instructions for gpt-4.1 on how to use the `apply_patch` tool.
pub const APPLY_PATCH_TOOL_INSTRUCTIONS: &str =
    include_str!("../templates/apply_patch_tool_instructions.md");

#[cfg(test)]
#[path = "apply_patch_contract_tests.rs"]
mod tests;
