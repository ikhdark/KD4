# Task Artifact Reference

Use this reference when creating or restructuring durable task state.

## Location And Naming

Store one task under:

```text
.codex/harness/runs/<yyyy-mm-dd>-<short-slug>/
```

Reuse an existing run for the same objective. Create only the files that carry
useful state; an empty artifact set is better than boilerplate.

## Artifact Menu

| File | Use it for | Minimum useful sections |
| --- | --- | --- |
| `PLAN.md` | A broad, risky, or resumable objective | Objective, non-goals, owner scope, risks, steps, validation intent |
| `IMPLEMENT.md` | Decisions or evidence that must survive | Current state, decisions, changed areas, validation receipts, open items |
| `EVAL.md` | Explicit capability or regression criteria | Criteria, grader, baseline, attempts, result, remaining risk |
| `QA_CHECKLIST.md` | Broad or risky review | Reviewed scope, findings, contract checks, validation, unresolved risk |
| `HARNESS_AUDIT.md` | Workflow or skill hardening | Scope, findings, evidence, top actions, decision |
| `HANDOFF.md` | Interruption, compaction, or later continuation | Objective, current state, decisions, evidence, touched files, next action, blockers |
| `ORCHESTRATOR.md` | Explicit agent coordination | Objective, owner, assignments, overlap constraints, integration order, proof |

Do not create every artifact by default. Prefer one `PLAN.md` for a resumable
task or one `HANDOFF.md` for continuation; add another file only when it owns
distinct information.

## Content Rules

- Record facts that are expensive to reconstruct: objective, user constraints,
  owner paths, material decisions, accepted deviations, proof, and next action.
- Link to source or generated evidence instead of pasting bulky content.
- Use stable symbols or paths when line numbers are likely to drift.
- Distinguish implemented, validated, skipped, blocked, and user-reported state.
- Record task-relevant dirty paths and competing owners without claiming they
  are complete.
- Delete placeholders and unused sections.

## Optional Metadata

Keep structured metadata small and human-readable when it materially helps:

```json
{
  "id": "2026-07-15-example",
  "status": "in_progress",
  "owner": "codex-rs/core",
  "created": "2026-07-15",
  "updated": "2026-07-15"
}
```

Do not introduce a schema or generator unless the user asks for automation.
Run directories are ignored local state by default; include an artifact in a
patch only when the user requests it as a deliverable.
