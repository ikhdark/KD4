# KD4 Harness Context Modes

Use context modes to keep a task's current phase explicit without loading a
large standing prompt into every turn.

## Research Mode

Use when the task is discovery, comparison, audit, or recommendation.

- Read broadly enough to support the claim.
- Keep edits out of scope unless the user asks to implement.
- Return findings first, then recommendations.
- Capture durable findings only when they will matter after compaction.

## Plan Mode

Use when the task is broad, risky, multi-step, or likely to resume later.

- Create or update `PLAN.md`.
- State non-goals and validation intent.
- Identify owner files, call paths, configs, generated artifacts, and tests.
- Keep the plan short enough to execute.

## Implementation Mode

Use when making focused changes.

- Inspect the nearest scoped `AGENTS.md` and owner path before editing.
- Use `apply_patch` for manual edits.
- Update `IMPLEMENT.md` only for decisions or evidence worth preserving.
- Avoid unrelated cleanup.

## Review Mode

Use when checking a patch, PR, or harness artifact.

- Lead with severity-ranked findings.
- Cite exact files and lines when possible.
- Separate proven issues from open questions.
- Use `QA_CHECKLIST.md` for broad or risky changes.

## Finish Mode

Use when closing out a task.

- Compare final claims against validation evidence.
- Record skipped checks with reasons.
- State remaining desktop publish or restart work when relevant.
- Write `HANDOFF.md` before stopping if future continuation is likely.

## Phase-Boundary Compaction

Compact or hand off at logical boundaries, not arbitrary token thresholds:

- research to plan: keep distilled findings and discard bulky exploration;
- plan to implementation: keep the accepted plan and validation route;
- implementation to test: keep changed files and known failure modes;
- failed approach to retry: keep the failure reason, then clear dead context;
- unrelated task switch: write a handoff before changing focus.

Do not compact mid-edit when recent file details, symbols, or partial reasoning
are still needed.
