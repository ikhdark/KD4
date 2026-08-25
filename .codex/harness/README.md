# KD4 Harness

This is KD4's lightweight workflow layer for planning, implementing, checking,
and resuming substantial work without changing product behavior by default.
[`workflow.md`](workflow.md) owns the lifecycle and completion-status definitions;
[`context-modes.md`](context-modes.md) owns phase and compaction guidance.

## Choose The Smallest Artifact Set

| Situation | Artifact |
| --- | --- |
| Focused task that fits in one turn | No durable artifact; follow the workflow in conversation |
| Multi-step, risky, or resumable task | [`templates/PLAN.md`](templates/PLAN.md) |
| Concurrent writers or validation lanes | [`templates/PREFLIGHT.json`](templates/PREFLIGHT.json) |
| Decisions or evidence must survive later turns | [`templates/IMPLEMENT.md`](templates/IMPLEMENT.md) |
| Behavior needs explicit capability or regression criteria | [`templates/EVAL.md`](templates/EVAL.md) |
| Broad or risky verification | [`templates/QA_CHECKLIST.md`](templates/QA_CHECKLIST.md) |
| Harness reliability review | [`templates/HARNESS_AUDIT.md`](templates/HARNESS_AUDIT.md) |
| Compaction, interruption, or task switch | [`templates/HANDOFF.md`](templates/HANDOFF.md) |
| Explicitly requested multi-agent work | [`templates/ORCHESTRATOR.md`](templates/ORCHESTRATOR.md) |

[`templates/HARNESS_CHECKLIST.md`](templates/HARNESS_CHECKLIST.md) is the compact
end-to-end checklist when a single task spans several of these concerns. Delete
unused placeholder sections instead of filling artifacts for completeness. Start
with one primary artifact and link to supporting artifacts instead of copying the
same facts, decisions, or validation results into several files.

Templates are durable source files, not per-task state. Copy each selected
template into `.codex/harness/runs/<yyyy-mm-dd>-<slug>/`, keep its conventional
basename, and fill the copy. Do not record task-specific state in `templates/`.

Resolve a preflight manifest with
`just workflow-preflight <manifest> <receipt>`. The command atomically
checks and publishes against the repository's active-receipt registry. Path,
named-contract, and Cargo target-lane overlap is reported as advisory metadata.
After copying the template, replace every `<...>` placeholder; paths in the
manifest are resolved relative to the copied manifest, not this template
directory.
Release it with `just workflow-preflight-release <assignment-id>` when the
assignment becomes terminal. The resolved receipt captures the
assignment and root-task identities, repository-lineage and concrete-workspace
identities, exact starting commit, a content-addressed tracked/untracked
workspace fingerprint, dependencies, owners, claims, validation commands,
canonical Cargo lane, and workspace strategy.

## Execution Rules

Apply the implementation, validation, and completion rules from the root
[`AGENTS.md`](../../AGENTS.md) and the nearest scoped instructions. The harness
records durable decisions and evidence; it does not define a second
implementation discipline. Follow a skill only when it is explicitly selected
or clearly matches the task. Using a skill alone does not require a harness run
directory.

Ordinary validation commands are check-only. Generated schemas and mirrors may
be changed only by their declared generated-output owner through the explicit
serialized regeneration recipe; for example,
`just config-schema-regenerate <owner>` or
`just app-server-schema-regenerate <owner>`.

## Generated Task State

If durable per-task artifacts are needed, place them under
`.codex/harness/runs/<yyyy-mm-dd>-<slug>/`. Treat those run directories as local
working state unless the user asks to keep them in a patch.

Do not add generated runtime logs, screenshots, binaries, or large transcripts
to reviewable changes unless they are explicitly requested.
