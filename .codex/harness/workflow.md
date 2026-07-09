# KD4 Harness Workflow

The KD4 harness is a small durable workflow layer for Codex work in this fork.
It does not replace the root `AGENTS.md`; it applies those rules in a repeatable
task shape.

## Principles

- Keep fork-local changes reviewable and easy to validate.
- Prefer current repository evidence over assumptions.
- Persist decisions only when they help future turns or future agents.
- Use `kd4-crosscheck-and-finish` as the execution discipline for harnessed
  implementation and repository-behavior work.
- Use Wiring Guard/KDWG as the static reachability proof layer for harnessed
  implementation changes when the plugin is active.
- Treat KD4 implementation work that activates Wiring Guard/KDWG,
  `wire-implementations`, or static wiring proof as harnessed in lightweight
  mode, even when no durable run directory is needed.
- Use subagents only when the user or active instructions explicitly ask for
  multi-agent work.
- Do not touch safety-sensitive surfaces unless the task names that surface.
- Do not claim desktop-visible completion without publish and restart evidence.
- Compact or hand off at phase boundaries, not in the middle of edits.

## Phase 1: Intake

1. Restate the concrete objective when it is ambiguous.
2. Choose the lightest valid task lane from the root `AGENTS.md`.
3. Identify the likely owner directory and nearest scoped `AGENTS.md`.
4. Decide whether durable artifacts are needed.

If Wiring Guard/KDWG, `wire-implementations`, or static wiring proof is
explicitly in scope for KD4 implementation work, the harness is active. The
artifact decision still depends on task size and audit value.

Use durable artifacts when a task is broad, interrupted, multi-step, multi-agent,
or likely to need a later audit. For a simple one-file edit, normal Codex
updates and a focused final answer are enough.

Use `context-modes.md` to switch between research, plan, implementation,
review, and finish modes without adding broad standing instructions.

## Phase 2: Plan

Create a `PLAN.md` artifact for substantial work. The plan should capture:

- objective and non-goals;
- lane and validation intent;
- files, configs, tests, and runtime entrypoints to inspect;
- risks, invariants, and open questions;
- a small milestone list.

Do not let planning become a substitute for inspecting the code.

For behavior that may regress, add an `EVAL.md` artifact with capability and
regression criteria before implementation.

## Phase 3: Implement

For implementation, debugging, refactoring, integration, migration, or
repo-behavior work, apply
`.codex/skills/kd4-crosscheck-and-finish/SKILL.md`. The harness records plan,
state, and evidence; `kd4-crosscheck-and-finish` controls the inspection,
full-path implementation, validation, and finish standard.

Before editing:

1. Read the nearest applicable `AGENTS.md`.
2. Inspect the owner files and relevant call path.
3. Check the nearest tests or validation route.
4. Note whether generated artifacts, schemas, publish paths, or desktop runtime
   behavior are involved.
5. Declare Wiring Guard/KDWG intent when the plugin is active and the task has
   implementation-shaped edits; use `--no-wiring-targets` only for docs,
   templates, planning, or config-only changes.

During edits:

- Use `apply_patch` for manual changes.
- Keep unrelated dirty-worktree changes intact.
- Keep generated files under the owning generator.
- Update the implementation log only when it preserves useful context.

## Phase 4: Check

Use the nearest sufficient proof:

- docs or skill-only changes: focused diff review and `git diff --check`;
- Rust source changes: focused crate checks or tests from `codex-rs`;
- schema or protocol changes: owning schema checks;
- script changes: syntax checks and closest script tests;
- publish or installed-binary changes: local publish dry-run or final publish;
- desktop-visible changes: publish, restart, process path/hash, and visible
  runtime evidence.

For implementation changes, run Wiring Guard/KDWG as the wiring proof layer
when active. The KD4 harness owns planning, evals, handoff, and validation
tracking; `kd4-crosscheck-and-finish` owns implementation discipline; Wiring
Guard owns static reachability proof. Use `--no-wiring-targets` only for docs,
templates, planning, or config-only changes.

Record skipped checks with the reason. Do not imply broader validation than was
actually run.

### Incomplete Implementation Finish Gate

Before claiming an implementation task is complete, perform this Codex-readable
finish gate:

1. Identify the intended runtime path for the change.
2. Confirm the changed code is actually reached from that path.
3. Sweep for no new or task-relevant placeholder/stub markers in changed code
   or the intended runtime path, including `TODO`, `FIXME`, `todo!()`,
   `unimplemented!()`, `stub`, `temporary`, `fake`, `mock-only`, and panic
   placeholders.
4. Check whether new public functions, types, config fields, commands, or
   workflow entries are wired into their expected callers.
5. Run the nearest sufficient validation, or explicitly state why validation
   was skipped or not applicable.
6. Report completion status using these definitions:
   - `passed`: runtime path checked, wiring checked, placeholder/stub sweep
     checked, nearest validation run or legitimately not applicable, and no
     known unverified completion risk.
   - `partial`: some evidence is missing or validation was skipped for a
     practical reason; Codex may summarize the work but must not claim the
     implementation is complete.
   - `blocked`: Codex cannot reasonably finish or verify without user input,
     missing environment, failing prerequisite, or unresolved design/ownership
     issue.

Final implementation answers must include:

- completion gate: passed / partial / blocked;
- wiring proof;
- validation run;
- remaining unverified risk.

Do not claim an implementation is complete when this gate is `partial` or
`blocked`.

When the task changes harness policy, skill behavior, automation, or validation
expectations, use `HARNESS_AUDIT.md` to score the change or capture follow-up
hardening work.

## Phase 5: Finish

The final answer should state:

- what changed;
- where the important files are;
- what validation ran;
- what remains unverified, if anything;
- whether desktop visibility still needs `just publish-local-codex-final` and a
  Codex Desktop restart.

Write `HANDOFF.md` before stopping if the task is unresolved, context-heavy, or
expected to resume in a later turn.

## Optional Multi-Agent Mode

Use the `ORCHESTRATOR.md` template only when multi-agent work is explicitly
requested or required by active instructions.

When multi-agent work is used:

- give each agent a bounded task and expected artifact;
- prevent recursive delegation unless it is explicitly needed;
- collect evidence before integrating results;
- keep one owner responsible for final validation and final answer.
