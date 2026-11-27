# Wall-clock profile

Terminal `TurnTiming` records carry `performanceObservations.wallClockProfile`
(`schemaVersion: 1`). The [owner](src/turn_timing/wall_clock.rs) derives it at
the terminal boundary, after the accepted-final tail is attached, from evidence
already in the same record: the activity timeline, blocking waits, the native
[dependency graph](../core/critical-path-timing.md), request/tool rows,
recovery telemetry and the final tail. It adds no timer, task or scheduling
change, and does not change `TIMING_SCHEMA_VERSION` or any existing field.
Sampling checkpoints do not carry it.

Records carry a `contract` reference to this file rather than repeating
definitions per turn. Every metric has a `status`: `available`, `partial` (computed from retained
evidence with the gaps listed in `gaps`), `unavailable` (with `reason`) or
`requires_matched_comparison` (one capture cannot supply it). Operation
endpoints are millisecond offsets; timeline and graph spans are nanoseconds on
the turn clock. Unions and operation sums are not additive with the exclusive
partition. No value is a proven saving, a necessity verdict or task acceptance.

With an incomplete tool inventory, the first observed material progress time is
an upper bound, the last is a lower bound, and post-completion overhead is an
upper bound. With no observed progress endpoint these metrics are unavailable.

Shared definitions:

- **Leaf executions**: model physical attempts (dispatch to completion or
  recorded owner end) and tool handler entry to exit for calls without
  recorded nested calls. These are elapsed operation spans, not CPU time or machine-only
  work. A request for user input can be a leaf spanning the wait for an answer or timeout;
  use the exclusive interactive partition to distinguish that time from machine activity.
- **Ready waits**: tool acceptance to handler entry, and continuation
  sampling ready to dispatch (observed only after tool results).
- **Critical path**: binding-predecessor walk (latest-finishing prerequisite)
  over a complete native dependency graph. Handoffs are elapsed gaps with
  unspecified cause; tool nodes include their internal waits. Segments of a
  Code Mode cell that emitted nested calls overlap those calls, so they are
  reported as `nestedCellNs` and never counted as execution work.

| Metric (`metrics.*`) | Definition |
| --- | --- |
| `criticalPathDuration` | Elapsed critical-path length with model/tool/nested-cell/wait/handoff/retry-handoff composition and the longest path nodes. `executionNs` is model plus leaf tool time. Requires a complete graph. |
| `exclusiveExecutionLatency` | Per class (`model_request`, `tool:NAME`): envelope minus the union of recorded nested calls. `turnNs` is time outside every model attempt and direct tool envelope. |
| `inclusiveExecutionLatency` | Per class: model attempt and tool acceptance-to-release envelopes. At most 32 classes plus `other`. |
| `inferenceWaitRatio` | Provider-active model union / turn duration; `exclusiveOnlyRatio` uses model-only time. |
| `toolWaitRatio` | Tool-active union / turn duration; `exclusiveOnlyRatio` uses tool-only time. |
| `schedulerInducedLatency` | Union of ready waits, with itemized first-poll, parallel-gate, authorization, pre-hook and pre-handler evidence components. |
| `synchronizationWaitTime` | Union of instrumented await suspensions (first Pending to Ready), split into resource contention and dependency waits. Open waits are censored at turn end; dropped intervals make it a lower bound. |
| `parallelExecutionEfficiency` | (leaf work / active union) / peak concurrency. |
| `effectiveConcurrency` | Leaf work / turn duration; `activeConcurrency` divides by the active union. |
| `executionOverlapRatio` | Time with at least two leaf executions / time with at least one; plus model/tool overlap from the exclusive partition. |
| `idleGapDuration` | Orchestration-phase time while a ready wait was pending and no leaf execution ran. Requires an exact activity timeline. |
| `criticalPathUtilization` | Model and leaf tool execution on the critical path / its elapsed length. |
| `workSpanRatio` | Model and leaf tool node work / execution-weighted longest dependency chain (nested cells and handoffs weigh zero). |
| `queueingDelayAttribution` | Per-source rows for resource-contention awaits, tool first-poll and authorization waits, the first-poll-to-admission envelope, continuation dispatch and host response-event queues. `dominantSource` ranks the retained positioned wait unions, excluding the admission envelope (legacy row key `tool:parallel_gate`, kind `first_poll_to_admission_envelope`); result/interrupt barriers stay under synchronization. |
| `latencyAmplificationFactor` | Turn duration / critical-path execution time. A cross-run baseline is a matched comparison. |
| `processInitializationBurden` | In-turn startup-prewarm, executor-readiness and router-build waits plus tool handler-entry-to-process-spawn launch. Components can overlap, so only lower/upper bounds are combined. Pre-turn startup and child-process initialization are outside this clock. |
| `timeToFirstMaterialProgress` | Earliest successful domain-tool completion or accepted final content. |
| `timeToLastMaterialProgress` | Latest such event. Domain classification is not task relevance; success is operational. |
| `postCompletionOverhead` | Turn end minus the last material progress event. |
| `wallClockRegressionAttribution` | `requires_matched_comparison`; carries the additive exclusive partition (`additive` when it sums to turn duration) and critical-path composition. |
| `parallelismOpportunityRatio` | Time with exactly one leaf execution while a ready wait was pending / time with exactly one. A ready operation cannot depend on unfinished work; resource and policy safety stay unverified. |
| `avoidableSerializationTime` | Ready waits while another execution was active. An upper bound that includes policy-required serialization. |
| `recoveryLatencyAmplification` | Union of failed model attempts/tool calls and their recovery (retry events, retried attempts, failure-diagnosis/repair generations); `amplificationFactor` divides by failure time; `criticalPathNs` counts recovery-owned critical-path nodes. Cancelled attempts are not failures. An observed envelope, not counterfactual added time. |
| `delegationBreakEvenLatency` | Per joined child: spawn tool envelope plus parent blocked wait (`breakEvenLatencyNs`), delegated span and parent concurrent time. Delegation saves elapsed time only if the child's own work exceeds the break-even; synthesis is excluded and child work needs the child capture. |
| `endToEndSpeedupEfficiency` | `requires_matched_comparison`: end-to-end saving / local component saving. |

KDR's `tools diagnose wall-clock-profile` projects these per turn and computes
the cross-run and child-capture parts from explicitly supplied captures; see
its [metric contract](../../../KDR/docs/metrics/WALL_CLOCK_PROFILE.md).
