---
name: kd4-harness
description: Create, maintain, audit, or use KD4-local harness artifacts under `.codex/harness`. Use when the user asks to create a harness system, start or plan a durable KD4 task, design agent/team workflows, update harness templates, define evals, audit harness quality, manage context handoff/compaction, capture implementation/check/finalization evidence, or work on KD4 implementation tasks that invoke Wiring Guard/KDWG, `wire-implementations`, or static wiring proof.
---

# KD4 Harness

Use this skill for KD4-local harness work: durable planning artifacts,
implementation logs, checklists, optional coordination templates, and workflow
guidance under `.codex/harness`.

Trigger this skill alongside Wiring Guard/KDWG for KD4 implementation work. In
that mode, default to the lightweight harness path: no run directory unless
durable state is useful, but still apply intake, check, and finish evidence
expectations.

## Required Reading

1. Read the root `AGENTS.md` instructions for KD4.
2. Read `.codex/AGENTS.md` for repo-local Codex file policy.
3. Read `.codex/harness/workflow.md`.
4. For implementation, debugging, refactoring, integration, migration, or
   repo-behavior work, read and apply
   `.codex/skills/kd4-crosscheck-and-finish/SKILL.md`.
5. For implementation changes, use the Wiring Guard/KDWG plugin
   (`wire-implementations`) as the static reachability proof layer when it is
   active.
   When this skill is selected because Wiring Guard/KDWG or
   `wire-implementations` is in scope, continue to apply the harness
   intake/check/finish gate even if no durable artifact is created.
6. Read only the reference file that matches the task:
   - `references/workflow.md` for lifecycle or harness usage questions.
   - `references/task-artifacts.md` for task/run artifact layout.
   - `references/agent-patterns.md` for multi-agent or orchestrator design.
   - `references/quality-gates.md` for validation and finish gates.
   - `references/audit-rubric.md` for harness scoring and hardening.
   - `references/evals.md` for capability or regression evals.
   - `references/context-management.md` for phase modes, handoff, and
     compaction.

## Operating Rules

- Keep the harness fork-local unless the user explicitly asks for upstream or
  distribution-ready behavior.
- Prefer guidance, templates, and reviewable artifacts before adding executable
  automation.
- Do not copy Trellis implementation code into KD4. Pattern-level adaptation is
  acceptable.
- The harness does not replace `kd4-crosscheck-and-finish`; implementation
  work inside a harnessed task must use that skill as the execution discipline.
- The harness does not replace Wiring Guard/KDWG; implementation work inside a
  harnessed task must use the plugin for static reachability proof when it is
  active.
- Treat any KD4 implementation task that explicitly uses Wiring Guard/KDWG,
  `wire-implementations`, or static wiring proof as harnessed at least in
  lightweight mode.
- Use subagents only when the user or active instructions explicitly ask for
  delegation or parallel agent work.
- Follow KD4's no-commit, no-publish default unless the user asks.
- For desktop-visible behavior, state whether local publish and Codex Desktop
  restart are still required.

## Common Tasks

### Create Or Update Harness Structure

1. Keep durable docs and templates under `.codex/harness`.
2. Keep this skill under `.codex/skills/kd4-harness`.
3. Update `.codex/AGENTS.md` when adding new durable or generated harness paths.
4. Validate docs-only changes with focused diff review and `git diff --check`.

### Start A Harnessed Task

1. Choose the lightest valid task lane from root `AGENTS.md`.
2. Apply `kd4-crosscheck-and-finish` when the task involves implementation or
   repository behavior.
3. Declare Wiring Guard/KDWG intent for implementation changes when the plugin
   is active.
4. Create a run directory only if durable state is useful.
5. Copy relevant templates from `.codex/harness/templates`.
6. Fill only the sections that materially help the task.
7. Keep generated logs and bulky runtime evidence out of reviewable changes
   unless requested.
8. Add `EVAL.md`, `HARNESS_AUDIT.md`, or `HANDOFF.md` only when the task needs
   explicit success criteria, hardening evidence, or resumable state.

### Finish A Harnessed Task

1. Confirm `kd4-crosscheck-and-finish` evidence was followed for implementation
   work.
2. Confirm Wiring Guard/KDWG proof ran when active and applicable, or that
   `--no-wiring-targets` was justified for docs, templates, planning, or
   config-only changes.
3. Confirm the implementation is wired through the intended owner path.
4. Run the nearest sufficient validation.
5. Record skipped checks and remaining risk.
6. Final response must match the evidence.

## References

- `references/workflow.md`
- `references/task-artifacts.md`
- `references/agent-patterns.md`
- `references/quality-gates.md`
- `references/audit-rubric.md`
- `references/evals.md`
- `references/context-management.md`
