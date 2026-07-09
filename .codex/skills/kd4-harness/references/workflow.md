# Workflow Reference

Use this reference when the user asks how the KD4 harness should operate or
when starting a harnessed task.

## Lifecycle

1. Intake: clarify the objective, classify the task lane, and identify the
   owner surface.
2. Plan: create a plan artifact only when the task benefits from durable state.
3. Implement: apply `kd4-crosscheck-and-finish`, then inspect owner files and
   call paths before editing.
4. Check: run the closest proof that exercises the touched behavior, plus
   Wiring Guard/KDWG proof when active and applicable.
5. Finish: report what changed, what passed, and what remains unverified.

## Wiring Guard Trigger

A KD4 implementation task that invokes Wiring Guard/KDWG,
`wire-implementations`, or static wiring proof triggers the harness in
lightweight mode. Durable artifacts remain optional; the intake, check, and
finish gates still apply.

## Lane Mapping

- Conversation lane: answer without repo inspection unless current files matter.
- Low-risk guidance lane: docs, templates, comments, or naming. Inspect the
  target file and nearest `AGENTS.md`.
- Focused code lane: narrow implementation, debugging, integration, or review.
  Use `kd4-crosscheck-and-finish`; inspect owner files, call path, configs,
  tests, and runtime entrypoints; use Wiring Guard/KDWG when active.
- Runtime-critical lane: safety-sensitive surfaces, generated artifacts,
  schemas, lockfiles, publish paths, dependency changes, desktop runtime, or
  broad refactors. Use `kd4-crosscheck-and-finish`, use Wiring Guard/KDWG when
  active, and require stronger proof.

## Artifact Threshold

Use durable artifacts for broad, interrupted, multi-step, multi-agent, or
auditable work. Avoid creating artifacts for tiny edits where the final answer
and focused diff are enough.

## Finish Standard

Do not claim completion from a partial skim, compile-only check, or unverified
runtime assumption. For implementation work, finish claims must satisfy
`kd4-crosscheck-and-finish` and Wiring Guard/KDWG when active. State exactly
what evidence supports the claim.
