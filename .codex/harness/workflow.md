# KD4 Harness Workflow

The KD4 harness is an optional durable-artifact layer for substantial work in
this fork. It preserves plans, decisions, evals, evidence, audits, and handoffs
without redefining repository implementation policy.

## Ownership Boundaries

- Root `AGENTS.md` owns task lanes, repository inspection, implementation
  discipline, validation selection, completion status, and final reporting.
- `.codex/harness` owns optional durable task artifacts and lifecycle guidance.
- `kd4-harness` routes requests for those artifacts; ordinary code changes do
  not need the skill.
- Use subagents only when the user or active instructions explicitly request
  delegation or parallel agent work.

## Phase 1: Intake

1. Confirm the concrete objective and choose the lane from root `AGENTS.md`.
2. Identify the owner directory, nearest scoped instructions, and validation
   route.
3. Decide whether durable state will materially help.

Use durable artifacts for broad, risky, interrupted, resumable, explicitly
auditable, or multi-agent work. For a focused one-turn task, keep the workflow
in conversation and create no run directory.

## Phase 2: Plan

Create `PLAN.md` only when durable planning is useful. Capture the objective,
non-goals, owner scope, validation intent, risks, and a short milestone list.
Add `EVAL.md` before implementation when capability or regression criteria need
to survive later turns.

## Phase 3: Implement

Follow root `AGENTS.md` and any active specialist skill. The harness may record
implementation decisions in `IMPLEMENT.md`, but that artifact does not replace
owner-path inspection or task-scoped validation.

Keep unrelated dirty changes intact. Keep generated output under its owning
workflow. Do not add logs, screenshots, binaries, or large transcripts to
reviewable changes unless requested.

## Phase 4: Check

Run the nearest sufficient proof required by root `AGENTS.md`, then record only
the evidence that matters for resumption or audit. Name skipped checks and their
reasons.

Use the completion-gate definitions and final-answer fields from root
`AGENTS.md`; do not maintain a second copy here. Use `QA_CHECKLIST.md` for broad
verification and `HARNESS_AUDIT.md` for harness-policy or skill changes.

## Phase 5: Finish

Summarize the material changes, focused validation, and remaining risk. Write
`HANDOFF.md` before stopping only when unresolved work or important context must
survive.

## Optional Multi-Agent Mode

Use `ORCHESTRATOR.md` only when multi-agent work is explicitly requested or
required by active instructions. Give each agent a bounded task and evidence
target, prevent recursive delegation unless requested, and keep one owner
responsible for final validation.
