---
name: kd4-harness
description: Create, maintain, audit, or use durable KD4 workflow artifacts under `.codex/harness`, including plans, implementation logs, evals, QA checklists, handoffs, context-management guidance, and explicitly requested agent/team workflows. Use for harness creation or maintenance, durable or resumable task state, harness audits, template changes, eval design, handoffs, or compaction; do not use as the default implementation skill for ordinary KD4 code changes.
---

# KD4 Harness

Use this skill only for KD4's durable workflow layer. Root `AGENTS.md` owns task
lanes, repository implementation discipline, validation, and completion
reporting. Independently applicable specialist skills keep their own triggers
and evidence requirements. The harness may record their results, but must not
redefine or activate them merely because ordinary implementation work is in
scope.

## Required Reading

1. Read the root `AGENTS.md` instructions for KD4.
2. Read `.codex/AGENTS.md` for repo-local Codex file policy.
3. Read `.codex/harness/README.md` and `.codex/harness/workflow.md`.
4. Read only the reference file that matches the task:
   - `references/task-artifacts.md` for task/run artifact layout.
   - `references/agent-patterns.md` for multi-agent or orchestrator design.
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
- Do not create a run directory unless durable state will help a broad,
  resumable, multi-step, multi-agent, or explicitly auditable task.
- For implementation inside an already harnessed task, follow root `AGENTS.md`
  and any active specialist skill. Record only evidence useful to the durable
  task state.
- Do not activate this skill solely because another specialist skill is active.
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
2. Create a run directory only if durable state is useful.
3. Copy relevant templates from `.codex/harness/templates`.
4. Fill only the sections that materially help the task.
5. For implementation, record the owner path, validation intent, and any
   independently applicable proof requirements.
6. Keep generated logs and bulky runtime evidence out of reviewable changes
   unless requested.
7. Add `EVAL.md`, `HARNESS_AUDIT.md`, or `HANDOFF.md` only when the task needs
   explicit success criteria, hardening evidence, or resumable state.

### Finish A Harnessed Task

1. Apply the completion and reporting gate from root `AGENTS.md`.
2. Record focused validation and any independently applicable specialist result.
3. Record skipped checks and remaining risk.
4. Write `HANDOFF.md` only when unresolved work or context must survive.
5. Keep final claims within the evidence actually recorded.

## References

- `references/task-artifacts.md`
- `references/agent-patterns.md`
- `references/audit-rubric.md`
- `references/evals.md`
- `references/context-management.md`
