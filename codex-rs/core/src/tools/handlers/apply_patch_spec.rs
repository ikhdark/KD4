use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::ToolSpec;

const APPLY_PATCH_LARK_GRAMMAR: &str = include_str!("apply_patch.lark");

/// Returns a custom tool that can be used to edit files. Well-suited for GPT-5 models
/// https://platform.openai.com/docs/guides/function-calling#custom-tools
pub fn create_apply_patch_freeform_tool(include_environment_id: bool) -> ToolSpec {
    let start = if include_environment_id {
        "start: begin_patch operation_id? (environment_id? revision_guard* hunk+ | retry) end_patch\nenvironment_id: \"*** Environment ID: \" filename LF\n"
    } else {
        "start: begin_patch operation_id? (revision_guard* hunk+ | retry) end_patch\n"
    };
    let definition = format!("{start}{APPLY_PATCH_LARK_GRAMMAR}");
    let mut description = "Use `apply_patch` to edit one or many files for one coherent change. This is a FREEFORM tool; do not wrap the patch in JSON.".to_string();
    if include_environment_id {
        description.push_str(" Put `*** Environment ID: <id>` after `*** Begin Patch` (or Operation ID when present) to select the target execution environment; it is required when multiple environments are available.");
    }
    description.push_str("\n\n");
    description.push_str("Optional `*** Operation ID: <id>` immediately after Begin Patch selects a durable retry identity for this dedicated tool, scoped to this thread and Codex home (1..128 ASCII letters/digits or _ . : -). Reuse it only with the identical patch, guards and execution scope: the original receipt is returned without mutation; conflicting reuse is rejected. Otherwise the call ID is used. Pending/unknown operations must be recovered through their owner, never reapplied under another ID. Recovered receipts are historical, not current file freshness. Environment ID, when needed, follows Operation ID.\n\n");
    description.push_str("A current read_file source hash supports exact range replacement of a region already fully read: `@@ codex-range START:END sha256:HASH` followed by one or more `+` lines (inclusive lines of the complete file; HASH is the whole-file source_sha256, never an excerpt hash; multiple ranges ordered and nonoverlapping). Do not add unprefixed blank separators. Use ordinary `-` hunks for deletions; empty range replacements are rejected. A stale hash is rejected before any write: read current source and reconcile before retrying. A patch built in JavaScript can be applied in the same cell as the read that feeds it.\n\n");
    description.push_str(codex_prompts::APPLY_PATCH_TOOL_INSTRUCTIONS);
    description.push_str("\n`diagnostics` lists advisory added-line findings (trailing whitespace, space-before-tab indentation, conflict markers) and possible Rust/Python syntax regressions using the existing tree-sitter parsers on committed pre/post-edit bytes. Syntax warnings require an increased recovery-error count; unchanged or lower counts can hide new errors. Unsupported languages are not checked; size, language-change or time limits report syntax_check_skipped. Warnings never reject a patch, and parsing is not compilation or correctness evidence. Direct display uses bounded counts and a labeled sample; canonical output and Code Mode retain full diagnostics at /diagnostics. Occupied move destinations carry destination_replaced plus displaced-byte metadata; exact overwritten_move_content is recoverable at /changes. Known changes_exact=false remains visible even on success. Conditional mutation refuses unsupported backends rather than falling back to an unsafe check/write. A recovery-required error names the retained displaced file; do not automatically replay or restore over a competing writer.");
    description.push_str("\n\nResults bind operation_id to the declared operation identity, or this tool call when omitted. outcome describes filesystem effects independently of success: committed means an exact nonempty committed delta (possibly a failed call's surviving prefix); unchanged means a successful exact no-op; failed means an unsuccessful call with an exact empty delta; unknown means changes_exact=false, even on success. Known changes each report outcome=committed; an empty inventory is not proof of no effects when outcome=unknown. multi_file_atomic is always false: publication is per file and rollback is best-effort. Revision guards are checked even for unchanged edits; byte-identical replacements skip writes, timestamp changes and mutation invalidation. Each committed replacement has a write_receipt {sha256, bytes} hashing the exact raw write buffer, including encoding/BOM and line endings. It identifies bytes written by this operation, not current file freshness; repeated writes remain ordered in changes. For moves it describes the destination. Deletions and moves to already-identical destinations have no write receipt. Failures include tool_failure with a stable code, effect certainty and safe recovery actions; details retain bounded expected/current excerpts, source hashes and nearby candidate locations when available. Candidates are advisory only, never permission to bypass guards or apply at a guessed site. In code mode patch failures remain results, not exceptions. The manifest consolidates repeated endpoints in first-publication order, retaining hunk_ordinals and operations, with operation, before/after raw-byte SHA-256 and byte count when known, and committed, unchanged, failed, unattempted or uncertain outcomes. Unknown and absent byte identities are distinct. Before/after identities span the whole operation, including rollback, rather than intermediate no-ops. Moves have separate source/destination entries; rolled_back marks confirmed restoration to the original bytes. Receipts come from the mutation owner, never later Git status, and describe this operation rather than current freshness. In code mode the result also includes success, text, changes (committed paths with kind and optional move_path), changes_exact, and environment_id. Exact successful updates return only hunk headers (post-edit line numbers); failures, uncertain deltas, deletions and add-overwrites include a bounded unified diff. Check success before continuing; if changes_exact is false, read the affected files before retrying.");
    description.push_str("\n`historical_reversal` is an advisory-only possible restoration of code removed in the last 32 path-relevant first-parent Git commits within 30 days. Matches cite removal commits and old line numbers; trimmed-line equality is not semantic equivalence. The bounded local/full-read lookup runs after publication without holding the mutation gate; missing history, unsupported environments or exhausted budgets report historical_reversal_check_skipped. Neither warnings nor skips change patch success, permissions or revision guards. Rename aliases and uncommitted history are not followed.");
    description.push_str("\nConfirmed no-effect single-source preflight failures may reuse their original diagnosis only after patch semantics, source revision and engine context match. This is not a replay of partial or uncertain writes. An explicit retained Retry Patch receipt intentionally re-verifies even without amendments.");
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

