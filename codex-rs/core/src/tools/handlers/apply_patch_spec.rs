use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::ToolSpec;

const APPLY_PATCH_LARK_GRAMMAR: &str = include_str!("apply_patch.lark");

/// Returns a custom tool that can be used to edit files. Well-suited for GPT-5 models
/// https://platform.openai.com/docs/guides/function-calling#custom-tools
pub fn create_apply_patch_freeform_tool(include_environment_id: bool) -> ToolSpec {
    let start = if include_environment_id {
        "start: begin_patch (environment_id? hunk+ | retry) end_patch\nenvironment_id: \"*** Environment ID: \" filename LF\n"
    } else {
        "start: begin_patch (hunk+ | retry) end_patch\n"
    };
    let definition = format!("{start}{APPLY_PATCH_LARK_GRAMMAR}");
    let mut description = "Use `apply_patch` to edit one or many files for one coherent change. This is a FREEFORM tool; do not wrap the patch in JSON.".to_string();
    if include_environment_id {
        description.push_str(" Put `*** Environment ID: <id>` immediately after `*** Begin Patch` to select the target execution environment; it is required when multiple environments are available.");
    }
    description.push_str("\n\n");
    description.push_str("A current read_file source hash supports exact range replacement of a region already fully read: `@@ codex-range START:END sha256:HASH` followed by one or more `+` lines (inclusive lines of the complete file; HASH is the whole-file source_sha256, never an excerpt hash; multiple ranges ordered and nonoverlapping). Do not add unprefixed blank separators. Use ordinary `-` hunks for deletions; empty range replacements are rejected. A stale hash is rejected before any write: read current source and reconcile before retrying. A patch built in JavaScript can be applied in the same cell as the read that feeds it.\n\n");
    description.push_str(codex_prompts::APPLY_PATCH_TOOL_INSTRUCTIONS);
    description.push_str("\n`diagnostics` lists advisory findings on added lines (trailing whitespace, space-before-tab indentation, conflict markers). Direct display uses bounded counts and a labeled sample; canonical output and Code Mode retain all locations at /diagnostics. Occupied move destinations carry destination_replaced plus displaced-byte metadata; exact overwritten_move_content is recoverable at /changes. Known changes_exact=false remains visible even on success. Conditional mutation refuses unsupported backends rather than falling back to an unsafe check/write. A recovery-required error names the retained displaced file; do not automatically replay or restore over a competing writer.");
    description.push_str("\n\nIn code mode the result is an object with success, text, changes (committed paths with kind and optional move_path), changes_exact, and environment_id. Exact successful updates return only hunk headers (post-edit line numbers); failures, uncertain deltas, deletions and add-overwrites include a bounded unified diff. Check success before continuing; if changes_exact is false, read the affected files before retrying.");
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

