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

## Opt-in owner journals and client hooks

Set `CODEX_RUNTIME_OBSERVATIONS=1` and `CODEX_RUNTIME_OBSERVATIONS_DIR` to an
existing absolute private capture directory before launching the process.
`protocol::timing::observations` retains content-free, sequence-numbered JSONL
with exact identities and the process clock, independently of terminal snapshots.
Each process creates its own journal (32 MiB, 1 MiB per row); overflow is explicit.
The numeric `client_probe` format and genuine provider token capture are unchanged.

The journal covers stream milestones, accepted/settled tools, nested dispatch and
promise settlement, validated `run_graph` node scopes, agent spawn/completion
joins, retry decisions, artifact recovery, final lifecycle boundaries, routing,
eligible request replay, bounded response attempts and shutdown joins. A dropped
scope is abandoned, not automatically a successful recovery or cancellation.
The terminal dependency graph remains the authoritative qualified task-path
owner; journal chronology alone does not make a complete causal graph.

`InProcessAppServerClient::next_event_with_observation` transfers a
`ClientRenderReceipt` with the exact streamed output occurrence. Receipt is
recorded before the facade consumer queue; dequeue is recorded at consumption.
Frontends call `scheduled(ui_id, active_sessions)` at actual UI dequeue and
`painted()` only after that output revision was painted. `painted_in(thread_id)`
also records the independently observed destination view for routing checks. This measures client
queue, UI scheduling and receipt-to-paint intervals on one process clock. It
does not infer Desktop rendering, paint coalesced-away revisions, or attribute
delay causally to the number of sessions. Other clients can use the same
endpoint hooks. Missing paint acknowledgements remain missing evidence.

Command launch hashes cover prepared arguments/cwd/environment and policy;
the command ledger also supplies its existing exact admission fingerprint.
These identities do not certify arbitrary filesystem/network state or prove
that repetition was a retry or unnecessary. Full logical prompt hashes remain
separate from physical wire-request hashes and workspace identity.

Sandbox preparation records both the await envelope and the actual blocking
worker, separately from process launch. Remote preparation is explicitly local
envelope construction, not remote sandbox work. Opaque socket transfer and
network delay remain unavailable without authoritative transport evidence;
provider compute/queue/reasoning timing still requires provider evidence.

Connectors can emit `codex_http_client::TransportIntervalObservation` through
`observe_transport_interval`, or retain `current_transport_interval_observer()`
when moving request-owned I/O to another task. The existing request telemetry
adapter binds the callback to the physical retry attempt. Local `Instant`
endpoints, wire-byte counts and static provenance flow into
`performanceObservations.transportIntervals` and the native journal. DNS, TCP,
TLS, upload/download and TCP round-trip callbacks are supported. The opaque
reqwest driver emits none: application waits never stand in for socket events.
Callbacks are request-bound and must not be reused for pooled-socket traffic
belonging to later requests. Invalid endpoints remain unavailable.

`measurementEvidence.requestOwnerClosures` records the sampling-future scope
end independently of successful provider completion, including abandoned/error
scopes. Counts and identities remain bounded and independently qualified by KDR;
scope cessation is never labeled as provider computation completion.

KDR exposes saved journals with `tools diagnose native-observations --capture
PATH` (repeat for separately clocked processes). Its existing app-server and
projection workflows accept `--runtime-observations`. Ordinary Windows streams
and tool measurement capture also accept `--observe-resources --resource-series`: a bounded, joined observer
of the existing owning job retains process lifetimes, parent observations,
CPU/private-memory/I/O samples and host-interface counters. No normal scan
samples processes. Exported resource captures use the existing KDR import/health
owner; counter resets, exited parents and short sample windows remain explicit.
The owning-job census is independent of measured member rows; initial CPU
baselines and unmeasured members remain unavailable rather than becoming zeros.
