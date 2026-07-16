---
name: kd4-harness
description: Preserve, resume, audit, or compact durable KD4 task context under `.codex/harness/runs`, including plans, implementation decisions, evals, QA evidence, handoffs, and explicitly requested agent coordination. Use for durable or resumable task state, harness or skill maintenance, workflow audits, eval design, context refreshes, handoffs, or compaction. Do not use for ordinary one-turn implementation that needs no durable artifact.
---

# KD4 Harness

Keep durable task context small, current, and subordinate to repository policy.
Treat root `AGENTS.md` as the authority for implementation, validation, safety,
and final reporting. Use this skill only to preserve workflow state or maintain
the workflow contract; do not create a second implementation discipline.

## Route Context

Read only the route needed for the request:

- Existing run: read the applicable instructions and the targeted artifact or
  section. Reload it immediately before updating shared state.
- New or restructured run: read `references/task-artifacts.md`.
- Context refresh, compaction, or handoff: read
  `references/context-management.md`.
- Harness or skill audit: read `references/audit-rubric.md`.
- Capability or regression eval: read `references/evals.md`.
- Agent coordination: read `references/agent-patterns.md` only when the user or
  active instructions explicitly request delegation or parallel agent work.

Treat instructions already present in the active context as read. Do not load
all references, generated logs, or unrelated run artifacts for completeness.

## Decide Whether To Persist State

Create or update `.codex/harness/runs/<yyyy-mm-dd>-<slug>/` only when the user
asks for durable workflow state or active instructions require it. Prefer no run
directory for a focused task that fits in the current conversation.

When a run is justified:

1. Create only the artifact files the task needs.
2. Preserve the objective, non-goals, current owner paths, material decisions,
   concise evidence, unresolved risk, and exact next action.
3. Reference source files and validation receipts; do not copy raw logs, large
   diffs, transcripts, screenshots, or generated output into the run.
4. Keep task-relevant user-owned dirty changes visible without treating dirtiness
   as proof of completion.
5. Update targeted sections instead of rewriting unaffected history.

## Work With A Harnessed Task

1. Read root and nearest scoped instructions required by the work.
2. Inspect the current owner and nearest proof before recording a plan.
3. Follow root `AGENTS.md` and any active specialist skill for implementation.
4. Record Wiring Guard/KDWG evidence only when that independent proof layer
   applies; this skill does not activate it.
5. Record focused validation after it runs, including skipped checks and the
   reason when that matters for resumption.
6. Refresh the handoff at a logical boundary when unfinished work must survive
   compaction, interruption, or a later turn.
7. Keep final claims within the evidence actually recorded.

## Maintain The Harness Contract

- Keep durable workflow guidance in this skill and its bundled references.
- Keep `.codex/harness/runs/**` as ignored local task state, not standing policy
  or template source.
- Keep frontmatter triggers precise and `agents/openai.yaml` aligned with the
  skill contract.
- Keep every bundled reference directly linked from this file and remove stale
  workspace-path references when source files move or disappear.
- Validate a changed skill with the installed `skill-creator` validator, a local
  reference-link check, and focused diff review.
- Forward-test only when active instructions permit subagents and deterministic
  validation cannot cover the behavior.

## Guardrails

- Keep the harness fork-local unless the user explicitly requests upstream or
  distribution-ready behavior.
- Use one writer for shared run state and reconcile current worktree evidence
  before promoting status.
- Use subagents only when the user or active instructions explicitly ask for
  delegation or parallel agent work.
- Preserve approval, sandbox, execution-safety, publish, and validation policy
  from the owning repository instructions.
- For desktop-visible work, preserve whether local publish, restart, process
  proof, and visible runtime evidence remain outstanding.

## References

- `references/task-artifacts.md`
- `references/context-management.md`
- `references/audit-rubric.md`
- `references/evals.md`
- `references/agent-patterns.md`
