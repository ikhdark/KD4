# Task Artifact Reference

Use this reference when creating or updating durable harness artifacts.

## Default Layout

For a task that needs durable state, use:

```text
.codex/harness/runs/<yyyy-mm-dd>-<slug>/
  PLAN.md
  IMPLEMENT.md
  HARNESS_CHECKLIST.md
  QA_CHECKLIST.md
```

Add `ORCHESTRATOR.md` only when multi-agent coordination is explicitly in
scope.

Add `EVAL.md`, `HARNESS_AUDIT.md`, or `HANDOFF.md` only when the task needs
explicit success criteria, harness scoring, or resumable context.

## Artifact Roles

- `PLAN.md`: objective, non-goals, lane, scope, assumptions, risks, milestones.
- `IMPLEMENT.md`: decisions, changed areas, deviations, validation evidence.
- `EVAL.md`: capability and regression criteria with grader evidence.
- `HARNESS_CHECKLIST.md`: KD4-specific readiness and finish checklist.
- `HARNESS_AUDIT.md`: scorecard for harness reliability and follow-up actions.
- `HANDOFF.md`: compact continuation state for later turns.
- `QA_CHECKLIST.md`: correctness, contract, runtime, and validation review.
- `ORCHESTRATOR.md`: optional coordination plan for explicitly delegated work.

## Task Metadata

If structured metadata is useful, keep it small and human-readable:

```json
{
  "id": "2026-07-09-example",
  "status": "planned",
  "lane": "focused-code",
  "owner": "codex-rs/core",
  "created": "2026-07-09"
}
```

Do not introduce a schema or generator until the user asks for automation.

## Reviewability

Run directories are local working state by default. Include them in a patch only
when they are the requested deliverable or when preserving them materially helps
future KD4 work.
