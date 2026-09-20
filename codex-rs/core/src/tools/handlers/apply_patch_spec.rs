use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::ToolSpec;

const APPLY_PATCH_LARK_GRAMMAR: &str = include_str!("apply_patch.lark");

/// Returns a custom tool that can be used to edit files. Well-suited for GPT-5 models
/// https://platform.openai.com/docs/guides/function-calling#custom-tools
pub fn create_apply_patch_freeform_tool(include_environment_id: bool) -> ToolSpec {
    let start = if include_environment_id {
        "start: begin_patch environment_id? hunk+ end_patch\nenvironment_id: \"*** Environment ID: \" filename LF\n"
    } else {
        "start: begin_patch hunk+ end_patch\n"
    };
    let definition = format!("{start}{APPLY_PATCH_LARK_GRAMMAR}");
    let mut description = "Use `apply_patch` to edit one or many files for one coherent change. This is a FREEFORM tool; do not wrap the patch in JSON.".to_string();
    if include_environment_id {
        description.push_str(" Put `*** Environment ID: <id>` immediately after `*** Begin Patch` to select the target execution environment; it is required when multiple environments are available.");
    }
    description.push_str("\n\n");
    description.push_str(codex_prompts::APPLY_PATCH_TOOL_INSTRUCTIONS);
    description.push_str("\n\nIn code mode the result is an object with success, text, changes (committed paths with kind and optional move_path), changes_exact, and environment_id. Check success before continuing; if changes_exact is false, inspect the filesystem before retrying. Large results use the standard recoverable tool-output projection.");
    ToolSpec::Freeform(FreeformTool {
        name: "apply_patch".to_string(),
        description,
        format: FreeformToolFormat {
            r#type: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition,
        },
    })
}

#[cfg(test)]
#[path = "apply_patch_spec_tests.rs"]
mod tests;
