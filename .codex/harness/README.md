# KD4 Harness

This directory holds the KD4-local harness system for durable agent work. It is
for planning, coordinating, implementing, checking, and finishing Codex tasks in
this fork without changing product behavior by default.

The harness combines useful patterns from the inspected local repositories:

- lightweight planning and checklist templates from the awesome harness
  materials;
- agent/team design patterns from the harness plugin materials;
- durable task-state and workflow ideas inspired by Trellis.

No Trellis implementation code is copied here. Keep this layer as KD4-local
guidance unless the user explicitly asks for executable automation.

## Contents

- `workflow.md`: the standard KD4 harness lifecycle.
- `context-modes.md`: lightweight phase modes and compaction guidance.
- `templates/PLAN.md`: planning artifact for non-trivial work.
- `templates/IMPLEMENT.md`: implementation log and decision record.
- `templates/EVAL.md`: capability and regression eval artifact.
- `templates/HARNESS_CHECKLIST.md`: completion and validation checklist.
- `templates/HARNESS_AUDIT.md`: scorecard for harness reliability reviews.
- `templates/HANDOFF.md`: resumable context before compaction or task switch.
- `templates/ORCHESTRATOR.md`: optional multi-agent coordination template.
- `templates/QA_CHECKLIST.md`: focused review and verification template.

## How To Use

Use the `.codex/skills/kd4-harness` skill when the user asks to create, update,
or run a harnessed KD4 task, or when KD4 implementation work explicitly invokes
Wiring Guard/KDWG, `wire-implementations`, or static wiring proof.

For implementation, debugging, refactoring, integration, migration, or
repo-behavior work, the harness must use
`.codex/skills/kd4-crosscheck-and-finish/SKILL.md` as the execution discipline.
The harness must also use Wiring Guard/KDWG as the static reachability proof
layer when the plugin is active. The harness stores durable planning and
evidence; it does not replace crosscheck-and-finish or Wiring Guard.

For routine implementation work, the harness can stay lightweight:

1. Classify the task lane using the root `AGENTS.md`.
2. Apply `kd4-crosscheck-and-finish` for inspection, implementation, and finish
   claims.
3. Declare Wiring Guard/KDWG intent before implementation edits when the plugin
   is active.
4. Treat Wiring Guard/KDWG-triggered tasks as harnessed even when no run
   directory is created.
5. Inspect the owner files, nearest call path, relevant config, and nearest
   tests before editing.
6. Use the templates only when they reduce ambiguity or preserve useful state.
7. Validate with the closest proof that exercises the touched behavior and the
   Wiring Guard/KDWG check when applicable.
8. Finish with evidence, remaining risk, and any desktop publish requirement.

Use `context-modes.md` when a task shifts between research, planning,
implementation, review, and finish phases. Use `HANDOFF.md` before compaction or
when a task is likely to resume later.

## Generated Task State

If durable per-task artifacts are needed, place them under
`.codex/harness/runs/<yyyy-mm-dd>-<slug>/`. Treat those run directories as local
working state unless the user asks to keep them in a patch.

Do not add generated runtime logs, screenshots, binaries, or large transcripts
to reviewable changes unless they are explicitly requested.
