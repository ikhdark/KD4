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

## Native graph and recovery inventories

Terminal timing also exports a typed `dependencyGraph` in the same clock domain.
Its model nodes use retained request IDs and dispatch/completion boundaries;
tool nodes use native execution IDs and acceptance/delivery boundaries. Edges
come only from the emitting-generation and consumed-result owners. Host handoff
boundary spans cover the intervening envelope, without assigning a CPU/wait cause.
Checkpoints omit the graph: an in-flight observation cannot certify task finish.

`complete` is independent of elapsed coverage. Missing identities/endpoints,
overflow, open processes, nested fork/join and model-prefix release remain
explicit `unavailableReasons`. In particular, `emitted_by` does not establish
that a tool waited for the *whole* model request to finish, even when rounded
timestamps coincide. Such graphs stay partial. Descendants are not claimed.
The current full-request graph contract cannot certify streaming-prefix or
arbitrary JavaScript task dependencies; those require authoritative segmented
fork/join evidence, not chronology or fabricated waits.

The existing `update_plan.measurements.dependencyNodes` annotation lane accepts
referenced repeat/input/work identity and necessity assessments for retained
native nodes. Validation precedes plan effects; publication uses the existing
plan owner. Labels cannot repair missing causal coverage or prove savings.

`measurementEvidence.artifactRecoveries` persists bounded read_tool_output
attempts, including failed/abandoned selections, with native clock endpoints
and selection/retention/delivery facts. Metadata-only audits are excluded.
The physical-model-retry inventory is unchanged; this separate inventory does
not claim exhaustive recovery, restored task correctness or causal added delay.

`measurementEvidence.nestedResults` reuses the complete byte count from the
actual Code Mode result serializer, before script/outer-display projection.
`native_nested_result_json_v1` measures that full JSON object, including wrapper
metadata, after the owning tool's per-result budget. It is not canonical artifact
bytes, displayed bytes, provider tokens, or an unbounded pre-owner-reduction
object. Counts and retention completeness are independent and bounded to 1,024.
