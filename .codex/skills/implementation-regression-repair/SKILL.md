---
name: implementation-regression-repair
description: Guide implementation work before edits and re-review existing implementations when regressions are suspected. Use during coding changes to understand the complete relevant execution path, preserve system-level invariants, implement the requested behavior directly, and correct only code-verified mistakes. Use the deeper regression-repair mode when the user explicitly asks to re-review completed, previously verified, or unreliable implementations. Do not use for style-only review, broad cleanup, speculative redesign, or building a new test suite.
---

# Implementation Correctness and Regression Repair

Help Codex implement requested changes correctly the first time and repair existing
implementations without redesigning correct code.

The skill may load before the first edit. Do not treat automatic loading as a
request for a broad audit. Follow the active implementation workflow unless the
user explicitly requests a re-review of existing or previously completed work.

## Core rules

- Understand the complete execution path relevant to the requested behavior before editing.
- Keep investigation bounded to what is necessary for correctness, but do not stop at a locally plausible function when system behavior depends on additional code.
- Trace the owning entry point, state flow, callers and callees, competing paths, side effects, persistence, cleanup, and relevant invariants when they affect the change.
- Implement the user's requested behavior directly once the path and intent are understood.
- Preserve correct behavior and unrelated local work.
- Require concrete code-level evidence before changing behavior beyond the user's request.
- Prefer the smallest safe edit that fixes the complete relevant path; do not confuse a small diff with incomplete implementation.
- Do not introduce broad refactors, new abstractions, architecture changes, formatting churn, dependency changes, or unrelated cleanup.
- Do not add or expand tests unless the user explicitly requests test coverage or active repository instructions require them.
- Report uncertainty instead of guessing when intent cannot be established.

## Active implementation mode

### 1. Establish intent and the complete relevant path

Before the first edit:

1. Identify the exact requested behavior, non-goals, and compatibility constraints.
2. Read the applicable repository instructions and the owning code.
3. Trace the complete relevant execution path far enough to establish:
   - where the behavior enters the system;
   - which code owns the decision or state;
   - how inputs, identity, authorization, configuration, or cached state reach it;
   - which callers, callees, registries, competing paths, or fallbacks can override it;
   - what side effects, persistence, cleanup, cancellation, or lifecycle behavior must remain correct.
4. Inspect existing tests or reproductions only when they clarify intent, reveal the defect, or provide a useful focused validation route.
5. Stop expanding when the behavior and all task-relevant owners are understood. Do not scan unrelated subsystems for completeness.

Do not create a formal inventory or classify every surrounding implementation.
Patch once the requested behavior and its complete relevant path are understood.

### 2. Implement the requested behavior

1. Change every task-relevant owner needed for the requested behavior to work end to end.
2. Correct the cause rather than adding compensating branches around a wrong state source, condition, identity, ordering, propagation, or cleanup step.
3. Preserve established public, stored-data, configuration, authorization, sandbox, installation, and compatibility behavior unless the user requested a change.
4. Do not bundle optional improvements with the necessary implementation.
5. Do not leave a competing or replaced path active when it would continue winning at runtime.
6. Keep the edit focused, but expand to an additional owner when code evidence shows it is required for correctness.

### 3. Post-edit correctness pass

After the relevant edits, re-read the complete affected path rather than only
the diff hunk. Check for mistakes introduced or exposed by the change:

- do not start tests or add new tests unless the user explicitly requested them.
- behavior that contradicts the requested intent;
- incomplete wiring or a new path that is never reached;
- mismatched state, identity, keys, fingerprints, or authorization context;
- stale or unsafe cache and reuse decisions;
- ordering, lifecycle, cancellation, concurrency, ownership, or cleanup errors;
- incorrect fallback or error-handling behavior;
- incompatible assumptions between callers and callees;
- partial migrations where old and new behavior conflict;
- diagnostic or exceptional-path code that regresses ordinary turns;
- code that appears locally correct but is overridden or invalidated elsewhere.

Correct only code-verified issues within the requested scope, do not expand into unrelated areas.