# Context Management Reference

Use this reference when a task is long, interrupted, phase-based, or likely to
hit context pressure.

## Modes

Research:
Gather evidence and return findings. Do not edit unless the user asks for
implementation.

Plan:
Distill evidence into `PLAN.md`, including non-goals and validation intent.

Implement:
Edit the focused owner path and record only useful decisions in `IMPLEMENT.md`.

Review:
Use severity-ranked findings and `QA_CHECKLIST.md` when risk is non-trivial.

Finish:
Match final claims to validation evidence. Write `HANDOFF.md` when the next
turn needs durable continuation context.

## Compaction Points

Good compaction points:

- after research, before planning or implementation;
- after plan acceptance, before focused edits;
- after a failed approach has been documented;
- before switching to an unrelated task;
- before ending a long task that will resume later.

Bad compaction points:

- in the middle of an edit;
- while recent line-level context is still needed;
- before a failure reason has been recorded;
- before saving the next concrete step.
