# Native diagnostic capture

Terminal `TurnTiming` snapshots retain diagnostic facts on the existing process
clock. These are observations, not task acceptance or achievable savings.

- `dependencyGraph`: native request/tool envelopes linked by recorded emission
  and consumption relations. Unknown prefix release, nested fork/join,
  missing endpoints, retention loss and incomplete closure prevent a complete
  graph. Descendant coverage is not asserted. Host handoffs are elapsed gaps
  with unspecified cause, not CPU or resource-wait measurements. Referenced
  `update_plan.measurements.dependencyNodes` annotations cannot supply missing
  causal edges or make an incomplete graph complete.
- `performanceObservations.transportPhases`: per-physical-HTTP-attempt phase
  receipts, including explicit unavailability and provenance. DNS, TCP, TLS,
  socket reuse and request-upload duration stay unknown when reqwest does not
  expose them. Prepared body size is not socket-upload size or time.
- `performanceObservations.connections`: observed WebSocket establishment,
  reuse selection, reconnection and failed setup envelopes. Local lifetime IDs
  survive transport adoption and reset with the actual transport. These are not
  isolated DNS/TCP/TLS timings or client receipt/paint acknowledgments.
- `performanceObservations.resources`: one Windows process-accounting interval
  between successful start/end samples: host-normalized CPU, ending private
  bytes and interval I/O byte counts. Creation time qualifies the process
  lifetime. Concurrent turns in the process are included; children and Desktop
  renderers are excluded. API failures or invalid counters produce no sample,
  not zeros. Sampling creates no timer, thread or background task.
- `measurementEvidence.operations`: native command-launch/read-call census,
  with owner execution/call IDs and lifecycle endpoints. Immutable snapshot
  reads retain exact delivered byte ranges and content/resource identities.
  Parsed selections and live byte pages without a whole-content hash remain
  unqualified. Multiple disjoint ranges remain explicit; the legacy single-
  extent metric contract is not falsely marked complete. Command history's
  observed revision hashes do not establish arbitrary external input state or
  authorize an exact retry/waste verdict.
- `measurementEvidence.nestedResults`: byte counts from the actual native
  Code Mode JSON-result serializer, before script/outer display projection.
  These differ from canonical artifact bytes and rendered response bytes.
- `measurementEvidence.artifactRecoveries`: bounded `read_tool_output` attempt
  identities, owner start/end times and selection/retention/delivery facts,
  including failed/abandoned attempts. Metadata-only audits are excluded. This
  is not an exhaustive recovery/network census or a task-progress assertion.
- `measurementEvidence.appServerTrace`: owner-recorded native accepted-tool
  request/result events, with unique emission IDs, monotonic sequence and a
  closed same-process observation window. KDR imports this through its existing
  trace validator and retention limits. It advertises AS-06 for qualified native
  inventories and AS-05 only for turns without recorded interactive waits.
  Missing/overflowed/unbound capture stays incomplete. Completeness requires
  reconciliation with the independent model-emitted request inventory;
  suppressed/unaccepted source requests and truncated source history prevent a
  complete declaration. Call IDs in this trace
  are native execution identities, not wire IDs reused by later generations.
  No terminal-emission, cancellation, client delivery, replay/resume/reload or
  shutdown guarantee is inferred from this narrower inventory.

Native inventories retain their limits and explicit dropped/saturation counts.
Ordinary timing, canonical/rendered output measurements and physical retry
telemetry remain separate. Sampling checkpoints do not repeat terminal-only
output inventories or certify a completed dependency graph.

## Controlled capture labels

An authorized runner can set these process environment variables before launch:

- `CODEX_DIAGNOSTIC_COHORT_ID` and `CODEX_DIAGNOSTIC_RUN_ID`
- `CODEX_DIAGNOSTIC_PROCESS_STATE`: `fresh_process`, `initialized_process`, or
  `reused_connection`
- `CODEX_DIAGNOSTIC_CACHE_COMPARISON_KEY` for explicitly matched request cohorts

Labels must be nonblank and at most 256 UTF-8 bytes. Omitted/invalid labels are
not inferred. Terminal publication supplies execution outcome. The runner must
still establish matching inputs/configuration and genuinely distinct repetitions;
labels alone are not evidence of experimental control.

Actual native-client paint, provider-private timing and the remaining AS event
capabilities need their respective endpoint owners. This capture does not
manufacture those observations from server flush or elapsed residuals.
