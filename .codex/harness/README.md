# KD4 Harness

This is KD4's lightweight workflow layer for planning, implementing, checking,
and resuming substantial work without changing product behavior by default.
[`workflow.md`](workflow.md) owns the lifecycle and completion gate;
[`context-modes.md`](context-modes.md) owns phase and compaction guidance.

## Choose The Smallest Artifact Set

| Situation | Artifact |
| --- | --- |
| Focused task that fits in one turn | No durable artifact; follow the workflow in conversation |
| Multi-step, risky, or resumable task | [`templates/PLAN.md`](templates/PLAN.md) |
| Decisions or evidence must survive later turns | [`templates/IMPLEMENT.md`](templates/IMPLEMENT.md) |
| Behavior needs explicit capability or regression criteria | [`templates/EVAL.md`](templates/EVAL.md) |
| Broad or risky verification | [`templates/QA_CHECKLIST.md`](templates/QA_CHECKLIST.md) |
| Harness reliability review | [`templates/HARNESS_AUDIT.md`](templates/HARNESS_AUDIT.md) |
| Compaction, interruption, or task switch | [`templates/HANDOFF.md`](templates/HANDOFF.md) |
| Explicitly requested multi-agent work | [`templates/ORCHESTRATOR.md`](templates/ORCHESTRATOR.md) |

[`templates/HARNESS_CHECKLIST.md`](templates/HARNESS_CHECKLIST.md) is the compact
end-to-end checklist when a single task spans several of these concerns. Delete
unused placeholder sections instead of filling artifacts for completeness.

## Execution Rules

Choose the task lane and apply the implementation, validation, and completion
rules from the root [`AGENTS.md`](../../AGENTS.md). The harness records durable
decisions and evidence; it does not define a second implementation discipline.
For implementation inside a harnessed task, follow any independently active
specialist skill. Another skill alone does not require a run directory or
activate the harness skill.

## Generated Task State

If durable per-task artifacts are needed, place them under
`.codex/harness/runs/<yyyy-mm-dd>-<slug>/`. Treat those run directories as local
working state unless the user asks to keep them in a patch.

Do not add generated runtime logs, screenshots, binaries, or large transcripts
to reviewable changes unless they are explicitly requested.
