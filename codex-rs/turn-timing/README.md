# Native diagnostic capture

Terminal `TurnTiming` snapshots retain diagnostic facts on the existing process
clock. These are observations, not task acceptance or achievable savings.

- `creditDelta`: observed balance change for one account, not turn billing.
  Requires two finite balance observations: a cached same-account baseline or
  the first fresh sample, followed by a fresh sample. Missing endpoints or a
  changed account leave the delta unavailable; equal observed endpoints are zero.
- `dependencyGraph`: schema-version-2 request/tool lanes segmented at recorded
  prefix-release and consumption boundaries, including qualified nested fork/join
  relations. Unknown prefix release or unrecorded nested dependencies,
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
- `performanceObservations.wallClockProfile`: terminal-only derived profile
  (critical path, inclusive/exclusive latency, wait ratios, concurrency,
  scheduling, queueing, idle gaps, material progress, recovery and delegation).
  It reads only this record's evidence; see the [metric contract](WALL_CLOCK_PROFILE.md).
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
telemetry remain separate. A result-bound Responses retry source identifies
the physical attempt that returned the triggering result. The optional
retry-decision fields `source_dispatch: "transport_entered"` and
`source_acceptance: "response_created"` record only positive facts. Entry is
after local transport admission, not a completed socket write. Acceptance
requires a WebSocket `response.created` with a valid nested response ID
admitted by that stream's response lifecycle; a metadata-only frame, HTTP
status, warmup or retry action does not qualify. HTTP acceptance, legacy
source attribution, and missing evidence leave the respective fields absent,
not false. These are source facts copied unchanged from the planned row to
its terminal row. Fallback may execute on HTTP after a WebSocket setup failure
with no source; its execution IDs and route describe the destination, never
source acceptance. Internal HTTP retries match the completed lower retry
ordinal; pre-entry failures and WebSocket handshakes have no sampling transport
source. Proven-unsent entries can still identify their own transport failures
without counting as provider usage. Sampling checkpoints do not repeat
terminal-only output inventories or certify a completed dependency graph.

For a matched retry capture, compare the same reconnectable WebSocket close
after a valid creation with the close after metadata only. The former has
both source fields; the latter has only entry. A creation-shaped frame with
only a top-level ID is not proof of acceptance. Join each plan and terminal
row on clock and decision ID, and compare its source attempt to the stream
that produced the failure, not the next attempt. An HTTP 503 after entry
has only source dispatch; a pre-entry WebSocket setup failure or fallback
has neither. Proven-unsent connection recovery jitters the cancellable wait around its
nominal 500 ms-to-15 s exponential schedule. The nominal state, total deadline,
provider-directed `Retry-After`, and separate WebSocket fallback cooldown are
unchanged. Cancellation during backoff can leave an unexecuted terminal
row without erasing the plan's source facts. The single reconnect after a
normal (1000) or going-away (1001) close waits on that jittered 500 ms
connection cadence; restart and try-again-later closes (1012/1013) and busy
responses keep the 5 s capacity backoff, and `Retry-After` still wins.

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
Each process creates a runtime journal (32 MiB) and separate `-mobile` (32 MiB),
`-lifecycle` (64 KiB retirement reserve, `mobile_connection_retired`) and
`-request` (4 MiB, `mobile_rpc_*` and `mobile_socket_*`) lanes, with a 1 MiB row
limit and explicit overflow: a lane ends with one `capture_overflow` row whose
reason is `row_too_large`, `byte_limit`, `segment_limit` or
`rollover_create_failed`, then records nothing further. With
`CODEX_RUNTIME_OBSERVATIONS_ROLLOVER=1` a full lane instead continues in
`-partNNNNNN` files, each with the lane's limit, up to 128 segments per lane;
rows then carry `journal_segment` and `previous_segment_last_id`. Retain all
available lane and part files. If a secondary lane cannot
open, its observations fall back to the runtime budget and rows identify
`unavailable_journal_lanes`; lane isolation is then unavailable. The
[journal owner](../protocol/src/timing/observations.rs) defines these limits.
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

Terminal diagnostics use the same opt-in journal and process clock:
`terminal_state_complete` brackets only the terminal snapshot's inner
state completion, with a flag for cached results. The snapshot admits this row
to the prepared bounded runtime-async lane; only a retained row is append
evidence. Core's `cancel_request` span also uses the prepared lane. A retained
pair observes scheduling only, not settlement, app-server acknowledgment, or
a complete capture. The start/end pairs for
`terminal_rollout_append` and `terminal_rollout_barrier` distinguish ordered
append acknowledgment from stable-storage acknowledgment; an abandoned span
does not establish cancellation of a queued write. These pairs are absent on
paths without terminal storage. The listener's `terminal_queue_pause` span
and the embedded `runtime_shutdown` span use the prepared runtime-async lane.
The former brackets the pause call, not proof of a queue change. The latter
observes the retained owner's result, not delivery of a shutdown response.
Pair only retained endpoints; queue admission is not an append receipt. Join an
interrupt's `cancel_ack_ready` to `bounded_response` using the connection and
request IDs on the same clock; those rows are not a client receipt.

For the ordinary process-output and post-exit drain timers, optional block
reason attribution does not wait for the timing collector lock. Their wake
records still carry the original deadline projected on the turn clock when
known. `measurementEvidence.processWaitAttribution` reports collector
contention losses observed at terminal capture. A missing marker does not
prove every prior observer ran; archive byte and inventory completeness
remain separate from reason attribution.

Connectors can emit `codex_http_client::TransportIntervalObservation` through
`observe_transport_interval`, or retain `current_transport_interval_observer()`
when moving request-owned I/O to another task. The existing request telemetry
adapter binds the callback to the physical retry attempt. Local `Instant`
endpoints, wire-byte counts and static provenance flow into
`performanceObservations.transportIntervals` and the native journal. DNS, TCP,
TLS, upload/download and TCP round-trip callbacks are supported. The local
reqwest/hyper-util/hyper-rustls forks emit real DNS resolution, individual
TCP connection attempts and Rustls handshakes, including proxy TLS and tunneled
destination TLS. No application wait stands in for one of these phases.
Raw TCP read/write callbacks include encrypted/framing bytes, pending-I/O waits
and potentially multiplexed traffic; they are explicitly `connectionScoped`
and must not be interpreted as a request's upload/download or TCP RTT. Each
connection retains at most 64 socket completions plus an overflow marker.
Pooled sockets and executor tasks retain weak observers, not turn owners. The
response body's observer lifetime permits observations after headers; a later
request never acquires attribution for an earlier connection's shared traffic.
Invalid endpoints, unsupported transports and missing phases remain unavailable.

`operations.schemaVersion: 2` retains the legacy whole-inventory gate and adds
independent `readInventory`/`commandInventory` counts and qualification. Immutable
reads retain all delivered ranges; KDR normalizes each delivery before counting
overlap with preceding successful reads. Nested endpoints use the actual runtime
result release, not the outer cell's later model delivery. Commands still do not
claim exact external filesystem/network input state from argv equivalence.

Owned `read_files` branches and their recovery continuations carry bounded
fork/join provenance without changing admission. Code Mode installs forwarding
proxies for `Promise.all`, `allSettled`, `race` and `any`: V8 runs the original
algorithm on the caller's arguments; reflection on a proxy can differ from
reflection on the original builtin. Awaiting the helper joins all recorded
branches, including rejected ones. Raw aggregates contribute edges only for
proven member reactions; unsupported cases remain unknown. `requestOwnerClosures` can
close a dependency lane when a sampling scope stops without provider completion;
its evidence reference and KDR endpoint basis preserve that distinction.

The optional `diagnostics/clientOutput` client notification carries a
content-free occurrence/revision, client-local receipt/dequeue/schedule/paint
times and observed target view. Server validation bounds identifiers and checks
ordering; it grants no thread access and never changes runtime execution.
`native-observations.remote_client_observations` keeps these client-reported
claims separate from server-verified delivery and in-process native paint tokens.
Real frontend integration is required; transport flush is not paint.

The app-server catalog records opt-in `hooks_list_*` spans in the runtime
journal. Group by the opaque operation ID and join each finish to its exact
start event; per-directory starts carry the original zero-based request index
(the server-default directory is index 0 when no directories are supplied).
Separate phases cover auth, configuration, workspace plugin policy, plugin
loading when allowed by both gates, and hook discovery. The discovery wait
includes blocking dispatch/join; the nested discovery span records the work
inside that task. Different directory indices can overlap. Configuration
includes cache and single-flight lookup; the workspace outcome is the effective
decision, not evidence of a network fetch instead of cache, a local default or
fail-open. No paths, wire request IDs, authentication data or hook content are
recorded. An assembled result does not attest client receipt.

Local stdio ingress uses an exact allowlist: `thread/list`, `thread/read`,
`thread/resume`, `thread/turns/list`, `thread/items/list`, `thread/queue/list`,
`thread/queue/add`, `thread/metadata/update`, `turn/start`, `config/read` and
`fs/getMetadata`. With journaling enabled, `local_rpc_received` records the
canonical method, server connection ID, versioned SHA-256 of the JSON wire
request ID, and an opaque `local:` operation ID for joining the applicable
admission, queue and handler events. These RPC observations do not log raw
wire IDs, request parameters, response bodies, or raw error messages.
`thread/items/list` can also emit the inline response-owner span;
`thread/queue/add` success sends its response from the queue owner instead.
A server boundary does not attest peer receipt or UI consumption.
Other local methods and non-stdio local transports do not enter this allowlist;
the independently owned remote-control observations are unchanged.

Remote-control requests additionally emit `mobile_rpc_received` and bounded
`mobile_rpc_admission`, `mobile_rpc_handler`, `mobile_file_read` and
`mobile_rpc_response` spans in the same opt-in journal. Opt-in remote `thread/resume` component spans use the mobile lane and the same
operation ID; local stdio retains its own `thread_resume_*` spans. An opaque occurrence
links ingress, dispatch, file reads and response attempts without recording wire
request IDs, paths, file contents, credentials or raw error messages. File reads
retain byte counts, OS/error-kind codes, limit/revision failures and filesystem
availability. The connection owner establishes remote-control provenance; client
names are not proof of a phone. Unknown ingress methods are recorded as `other`.
These are server boundaries, not relay-write, phone-receipt, tap or preview
acknowledgements. A response owner returning does not establish delivery. KDR's
`tools diagnose native-observations --capture PATH` exposes these under `mobile`;
legacy journals remain unavailable for that coverage. Missing requests alone
cannot establish that the phone ignored a tap.
Remote-control virtual-client retirement also emits `mobile_connection_retired`
with the server connection ID and a static reason (peer close, reinitialize,
idle expiry, outbound stop, transport retirement, or initialize-forward failure).
KDR exposes these under `mobile.owner_events`. This journal boundary survives
missing terminal SQLite log rows; it records the server's retirement decision,
not the initiating phone/network cause or successful close-event forwarding.

### Mobile backend admission

`mobile_backend_admission` (schema 1) is an opt-in app-server startup receipt,
recorded before remote-control work starts. Join it by journal PID and unique
`clock_id`, not by PID or wall time alone. `monotonic_before_ns` and
`monotonic_after_ns` bracket the `unix_ns` sample in that clock domain. It does
not replace the existing transport's `mobile_backend_epoch` or Desktop receipt.

`executable_sha256` is the process build hash, not publication metadata. A
`pinned_running_image` hash is bound to the Linux running image; a
`current_executable_path_snapshot` is best effort and must be matched against
an independently pinned stable executable for exact build attribution. A null
hash means unavailable. `startup_policy_sha256` identifies only the
`app_server_startup_policy.v1` projection of the resolved startup: transport
kind, startup mode and enforced remote-control policy, runtime source and plugin
choices, analytics default resolution, validation, residency, approval,
permission/network categories, login-shell policy, web search, and sorted
code-defined feature keys. No layer text, paths, provider/endpoint strings,
filesystem rules, credentials or other free-form values enter the digest. Equal
hashes do not attest equal full configurations or later runtime overrides. If
startup fell back after a configuration error, `startup_config_resolved` is
false and the projection hash is null. This is not proof of phone activity,
remote-control admission after authentication, or archive completeness.

The editable sibling Codex-Fork supplies opt-in transcript receipt/application
observations through the existing privileged host connection. Initialization
advertises diagnostics schema/journal availability. Journaled live assistant-text
deltas optionally carry `sourceOccurrence`; their UTF-16 subranges survive host
coalescing and splitting. Active-prefix recovery carries `replayed: true` and no
live occurrence identity. Route journals retain thread/turn/item/source identity.
These are distinct source, delivery, application and visibility boundaries;
neither client reports nor the existing visibility proxy prove compositor paint.
See [the frontend owner](../../../Codex-Fork/frontend/README.md) for explicit
capture, privacy, installation and legacy/other-output limitations. Backend
journaling remains launch-time opt-in and can add per-emission I/O overhead.

Independent owner journals include terminal emission/writer outcome spans,
cancel claim and startup fences, replay candidate inventories, command approval
submission and bounded shutdown inventories/dispositions. Timeouts do not imply
that retained cleanup owners were aborted. Optional durable index checkpoints
and reload streams use the same normalized-record digest before payload
hydration. Payload references retain their existing hashes; payload bytes are
not rehashed. Checkpoints are bounded to 64 MiB and reuse unchanged writer
observations; cache invalidates on append/reopen. Capture errors do not change
durability outcomes. Reload streams are not declared atomic snapshots, and
these owner facts do not automatically advertise additional AS guarantees.

`measurementEvidence.requestOwnerClosures` records the sampling-future scope
end independently of successful provider completion, including abandoned/error
scopes. Counts and identities remain bounded and independently qualified by KDR;
scope cessation is never labeled as provider computation completion.

KDR exposes saved journals with `tools diagnose native-observations --capture
PATH` (repeat for separately clocked processes). Its existing app-server and
projection workflows accept `--runtime-observations`. The app-server workload
(`KDR/tools/benchmark_app_server_backpressure.py`) also accepts
`--observe-resources --resource-series` for ordinary Windows streams and tool
measurement capture: a bounded, joined observer
of the existing owning job retains process lifetimes, parent observations,
CPU/private-memory/I/O samples and host-interface counters. No normal scan
samples processes. Exported resource captures use the existing KDR import/health
owner; counter resets, exited parents and short sample windows remain explicit.
The owning-job census is independent of measured member rows; initial CPU
baselines and unmeasured members remain unavailable rather than becoming zeros.
