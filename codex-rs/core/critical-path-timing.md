# Critical-path timing evidence

Introduced in timing schema 36, optional `blocking` evidence extends the existing
process-clock `activityTimeline`. It does not change scheduling or admission.

- An interval starts only when an instrumented future returns Pending and ends
  when it returns Ready. Cancellation closes it with `cancelled: true`; an open
  endpoint is censored at the frozen timeline boundary. Await envelopes include
  wake/re-poll latency, not CPU execution or synchronous preparation.
- Resource-contention observers cover shared/exclusive/cohort/scoped workspace
  admission, diff/evidence tracker acquisition, sampling admission and cooperative
  `agent_lock` waits. Most resource prerequisites name classes, not particular
  holders or repositories. `agent_lock` prerequisites additionally retain the
  resource, observed holder and acquisition token from the lock owner's atomic
  busy/subscription snapshot; this is historical attribution, not current ownership.
- Dependency-wait observers cover ordered tool-result collection and interrupt
  resolution. Explicit edges also link model-issued (direct) tool executions to
  their emitting generation and a resumed generation to model-visible tool
  results it consumes. Nested Code Mode calls get no generation edge.
- IDs and nanosecond offsets are turn-relative in the enclosing clock domain.
  Checkpoints upsert intervals by ID and append new edges; lost-process recovery
  merges them the same way. Terminal timing keeps the full retained evidence,
  even when detailed tool lifecycle events spill.
- Retention is bounded to 2,048 waits and 8,192 edges per turn. Dropped counts
  stay explicit, including edges skipped with a dropped wait or a capped tool-call
  row. Legacy absence is unknown, not zero contention.

KDR's `tools diagnose critical-path` exposes this evidence separately from a
complete task dependency graph. It distinguishes resource contention, dependency
waits, observed process execution, model latency and tool latency. Process flight
is not CPU time; model/tool envelopes include internal waits. Category unions
overlap and must not be added. Uninstrumented dependencies, remote descendants
and unattributed holder identities remain unknown; these records alone do not
prove a full causal critical path or achievable scheduling savings.
