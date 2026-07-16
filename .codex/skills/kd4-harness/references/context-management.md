# Context And Handoff Reference

Use this reference when a task is long, interrupted, phase-based, or likely to
hit context pressure.

## Load Current Context

Load authoritative context in this order and stop when the task is grounded:

1. The latest user objective and constraints.
2. Root and nearest scoped instructions not already present in active context.
3. The targeted run artifact or handoff, if one exists.
4. The current owner source and nearest test or validation route.
5. `SOURCEMAP.md` only for ambiguous ownership, cross-cutting work, or a
   runtime-to-install trace.
6. A specific generated receipt only when the claim depends on it.

Do not reload all harness references, all run files, or `.codex/verify-local`
logs merely because they exist. Treat the current worktree as source of truth
when a handoff and source disagree.

## Refresh Durable Context

After a material decision or phase boundary, preserve only information needed
to resume safely:

- objective and non-goals;
- user constraints and task-relevant dirty changes;
- current owner paths and intended runtime path;
- decisions and rejected approaches with reasons;
- completed work and focused validation receipts;
- exact next action;
- blockers, unresolved questions, and remaining risk.

Do not preserve raw logs, full transcripts, speculative branches, redundant
policy text, or stale status. Verify drift-prone facts before carrying them into
a new handoff.

## Handoff Shape

Write `HANDOFF.md` with these sections when continuation state is required:

1. Objective
2. Current state
3. Decisions and rejected approaches
4. Touched files and ownership notes
5. Proven evidence and skipped checks
6. Next concrete action
7. Blockers and remaining risk

## Compaction Boundaries

Compact after research, an accepted plan, a completed edit, a documented failed
approach, or before a task switch. Do not compact mid-edit, before preserving a
failure reason, or while recent line-level context is still required.
