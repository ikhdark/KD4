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
- Hook-free passive process polls can join the native read cohort. Same-handle
  interaction remains ordered by the process manager; input, EOF and termination
  are not passive polls. Outside a repository, hook-free commands with the same
  shell-aware read-only proof that grants shared repository admission also join
  the cohort instead of taking the global exclusive gate one at a time; writers
  and unproven commands still exclude them.
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

The terminal [wall-clock profile](../turn-timing/WALL_CLOCK_PROFILE.md) derives
critical-path duration, utilization and work-span ratio from the complete native
graph below; an incomplete graph leaves them unavailable.

## Native graph and recovery inventories

Terminal timing also exports a typed `dependencyGraph` in the same clock domain.
Its model nodes use retained request IDs and dispatch/completion boundaries;
tool nodes use native execution IDs and acceptance/delivery boundaries. Edges
come only from the emitting-generation and consumed-result owners. Host handoff
boundary spans cover the intervening envelope, without assigning a CPU/wait cause.
Checkpoints omit the graph: an in-flight observation cannot certify task finish.

A rollout record timestamp describes persistence order, not a shared execution endpoint. A native command completion, the enclosing Code Mode cell, nested publication, and the outer response can be recorded in a different order. Join by call, cell and attempt identities and use the matching native clock endpoints for durations; do not subtract heterogeneous timestamps to infer handler time or clamp an invalid interval to zero.

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

Code Mode callers that will select retained output can set `artifact_only_on_success: true` on `exec_command` to omit inline streams on a completely retained, noninteractive terminal success. The bounded display and artifact locator remain available. `rendered_view_bytes` includes generated notices; `delivered_view_bytes` counts verbatim normalized UTF-8 in the view when known. Neither is the total producer byte count. For oversized nested launch receipts, the export keeps that last measurement alongside the full receipt reference, keyed by the command output artifact ID. Failures, incomplete capture and live handles retain their normal projection.

`measurementEvidence.nestedResults` reuses the complete byte count from the
actual Code Mode result serializer, before script/outer-display projection.
`native_nested_result_json_v1` measures that full JSON object, including wrapper
metadata, after the owning tool's per-result budget. It is not canonical artifact
bytes, displayed bytes, provider tokens, or an unbounded pre-owner-reduction
object. Counts and retention completeness are independent and bounded to 1,024.

Final-answer tail schema 1 measures host observations. An accepted message has an
item ID; its first/full times use matching item and digest observations when
available. A directly surfaced tool answer has no message item ID and records
the complete-text observation as both endpoints. Equal timestamps do not measure
generation time, and neither path establishes native renderer acknowledgment.
The optional `answerOrigin` and `measurementBasis` distinguish these cases.
For explicit Code Mode delivery, `sourceCallId` identifies the delivering call;
`sourceSamplingRequestId` is present only when that call and its generation
uniquely identify a recorded sampling request.
